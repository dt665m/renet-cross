use std::{net::AddrParseError, time::Duration};

use futures_channel::mpsc;
use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, ArrayBuffer, Reflect, Uint8Array};
use renet::{ConnectionConfig, RenetClient};
use renetcode::{ClientAuthentication, NetcodeClient, NetcodeError};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState, RtcDataChannelType, RtcIceGatheringState,
    RtcConfiguration, RtcIceServer, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

use crate::SessionCreateResponse;

const ICE_GATHER_TIMEOUT_MS: u32 = 8_000;
const DATA_CHANNEL_OPEN_TIMEOUT_MS: u32 = 30_000;
const STATE_POLL_INTERVAL_MS: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum WebRtcClientError {
    #[error("HTTP request to {url} failed: {detail}")]
    HttpRequest { url: String, detail: String },
    #[error("HTTP {status} from {url}: {body}")]
    HttpStatus { url: String, status: u16, body: String },
    #[error("invalid JSON from {url}: {detail}")]
    HttpDecode { url: String, detail: String },
    #[error("invalid webrtc_addr in session response '{addr}': {source}")]
    InvalidSessionWebRtcAddr { addr: String, source: AddrParseError },
    #[error("session client_id ({session_client_id}) did not match answer client_id ({answer_client_id})")]
    ClientIdMismatch {
        session_client_id: u64,
        answer_client_id: u64,
    },
    #[error("missing or empty local SDP after offer creation")]
    MissingLocalSdp,
    #[error("ICE gathering timed out after {timeout_ms}ms")]
    IceGatherTimeout { timeout_ms: u32 },
    #[error("data channel failed to open within {timeout_ms}ms")]
    DataChannelOpenTimeout { timeout_ms: u32 },
    #[error("data channel is not usable (state={state})")]
    DataChannelState { state: &'static str },
    #[error("javascript error: {0}")]
    Javascript(String),
    #[error(transparent)]
    Netcode(#[from] NetcodeError),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SdpHttpOfferRequest {
    sdp: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SdpHttpAnswerResponse {
    client_id: u64,
    sdp: String,
}

pub struct WebRtcNetcodeClientTransport {
    _peer: RtcPeerConnection,
    data_channel: RtcDataChannel,
    netcode_client: NetcodeClient,
    inbox: mpsc::UnboundedReceiver<Vec<u8>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl WebRtcNetcodeClientTransport {
    pub fn update(&mut self, duration: Duration, client: &mut RenetClient) -> Result<(), WebRtcClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            client.disconnect_due_to_transport();
            return Err(WebRtcClientError::Netcode(NetcodeError::Disconnected(reason)));
        }

        if client.disconnect_reason().is_some() && !self.netcode_client.is_disconnected() {
            if let Ok((_addr, packet)) = self.netcode_client.disconnect() {
                let packet = packet.to_vec();
                let _ = self.send_data_channel_packet(&packet);
            }
        }

        loop {
            match self.inbox.try_next() {
                Ok(Some(mut packet)) => {
                    log::trace!("web transport received datachannel packet bytes={}", packet.len());
                    if let Some(payload) = self.netcode_client.process_packet(&mut packet) {
                        client.process_packet(payload);
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if self.netcode_client.is_connected() {
            client.set_connected();
        } else if self.netcode_client.is_connecting() {
            client.set_connecting();
        }

        if let Some((packet, _addr)) = self.netcode_client.update(duration) {
            let packet = packet.to_vec();
            log::trace!("web transport sending netcode packet bytes={}", packet.len());
            self.send_data_channel_packet(&packet)?;
        }

        Ok(())
    }

    pub fn send_packets(&mut self, client: &mut RenetClient) -> Result<(), WebRtcClientError> {
        if let Some(reason) = self.netcode_client.disconnect_reason() {
            return Err(WebRtcClientError::Netcode(NetcodeError::Disconnected(reason)));
        }

        let packets = client.get_packets_to_send();
        for packet in packets {
            let (_addr, payload) = self.netcode_client.generate_payload_packet(&packet)?;
            let payload = payload.to_vec();
            log::trace!(
                "web transport sending renet payload bytes={} netcode_bytes={}",
                packet.len(),
                payload.len()
            );
            self.send_data_channel_packet(&payload)?;
        }

        Ok(())
    }

    fn send_data_channel_packet(&self, payload: &[u8]) -> Result<(), WebRtcClientError> {
        match self.data_channel.ready_state() {
            RtcDataChannelState::Open => self
                .data_channel
                .send_with_u8_array(payload)
                .map_err(js_error),
            RtcDataChannelState::Connecting => Ok(()),
            state => Err(WebRtcClientError::DataChannelState {
                state: data_channel_state_label(state),
            }),
        }
    }
}

pub async fn connect_via_sdp_http(
    base_http: &str,
    protocol_id: u64,
) -> Result<(RenetClient, WebRtcNetcodeClientTransport, u64), WebRtcClientError> {
    connect_via_sdp_http_with_overrides(base_http, protocol_id, None).await
}

pub async fn connect_via_sdp_http_with_overrides(
    base_http: &str,
    protocol_id: u64,
    webrtc_addr_override: Option<&str>,
) -> Result<(RenetClient, WebRtcNetcodeClientTransport, u64), WebRtcClientError> {
    log::info!("starting web session bootstrap against {base_http}");
    let session = create_session(base_http).await?;
    log::info!(
        "web session response: client_id={} udp_addr={} webrtc_addr={} webrtc_offer_url={}",
        session.client_id,
        session.udp_addr,
        session.webrtc_addr,
        session.webrtc_offer_url
    );

    let stun_urls = Array::new();
    stun_urls.push(&JsValue::from_str("stun:stun.l.google.com:19302"));
    stun_urls.push(&JsValue::from_str("stun:stun1.l.google.com:19302"));

    let ice_server = RtcIceServer::new();
    ice_server.set_urls(&JsValue::from(stun_urls));

    let ice_servers = Array::new();
    ice_servers.push(&JsValue::from(ice_server));

    let config = RtcConfiguration::new();
    config.set_ice_servers(&JsValue::from(ice_servers));

    let peer = RtcPeerConnection::new_with_configuration(&config).map_err(js_error)?;
    let data_channel_init = RtcDataChannelInit::new();
    data_channel_init.set_ordered(false);
    data_channel_init.set_max_retransmits(0);
    let data_channel = peer.create_data_channel_with_data_channel_dict("renet", &data_channel_init);
    data_channel.set_binary_type(RtcDataChannelType::Arraybuffer);

    let offer_js = JsFuture::from(peer.create_offer()).await.map_err(js_error)?;
    let offer_sdp = Reflect::get(&offer_js, &JsValue::from_str("sdp"))
        .map_err(js_error)?
        .as_string()
        .ok_or(WebRtcClientError::MissingLocalSdp)?;
    if offer_sdp.trim().is_empty() {
        return Err(WebRtcClientError::MissingLocalSdp);
    }

    log::debug!("created local WebRTC offer ({} bytes)", offer_sdp.len());

    let local_offer = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    local_offer.set_sdp(&offer_sdp);
    JsFuture::from(peer.set_local_description(&local_offer))
        .await
        .map_err(js_error)?;

    match await_ice_complete(&peer).await {
        Ok(()) => {}
        Err(WebRtcClientError::IceGatherTimeout { timeout_ms }) => {
            if local_sdp_has_candidates(&peer) {
                log::warn!(
                    "ICE gathering timed out after {}ms; continuing with partial local SDP that already has candidates",
                    timeout_ms
                );
            } else {
                return Err(WebRtcClientError::IceGatherTimeout { timeout_ms });
            }
        }
        Err(err) => return Err(err),
    }

    let local_description = peer.local_description().ok_or(WebRtcClientError::MissingLocalSdp)?;
    let local_sdp = local_description.sdp();
    if local_sdp.trim().is_empty() {
        return Err(WebRtcClientError::MissingLocalSdp);
    }

    let answer = post_offer(&session.webrtc_offer_url, local_sdp).await?;
    if answer.client_id != session.client_id {
        return Err(WebRtcClientError::ClientIdMismatch {
            session_client_id: session.client_id,
            answer_client_id: answer.client_id,
        });
    }

    log::debug!("received SDP answer for client_id={} ({} bytes)", answer.client_id, answer.sdp.len());

    let remote_answer = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    remote_answer.set_sdp(&answer.sdp);
    JsFuture::from(peer.set_remote_description(&remote_answer))
        .await
        .map_err(js_error)?;

    await_data_channel_open(&data_channel).await?;
    log::info!("web data channel is open for client_id={}", answer.client_id);

    let (tx, rx) = mpsc::unbounded::<Vec<u8>>();
    let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
        if let Some(bytes) = message_bytes(&event) {
            log::trace!("web data channel message bytes={}", bytes.len());
            let _ = tx.unbounded_send(bytes);
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    data_channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let selected_webrtc_addr = webrtc_addr_override
        .filter(|addr| !addr.trim().is_empty())
        .unwrap_or(&session.webrtc_addr);
    if selected_webrtc_addr != session.webrtc_addr {
        log::info!(
            "overriding session webrtc_addr '{}' with configured '{}'",
            session.webrtc_addr,
            selected_webrtc_addr
        );
    }

    let server_addr = selected_webrtc_addr
        .parse()
        .map_err(|source| WebRtcClientError::InvalidSessionWebRtcAddr {
            addr: selected_webrtc_addr.to_owned(),
            source,
        })?;

    let authentication = ClientAuthentication::Unsecure {
        protocol_id,
        client_id: session.client_id,
        server_addr,
        user_data: None,
    };

    let netcode_client = NetcodeClient::new(browser_now_duration(), authentication)?;
    let renet = RenetClient::new(ConnectionConfig::default());

    let transport = WebRtcNetcodeClientTransport {
        _peer: peer,
        data_channel,
        netcode_client,
        inbox: rx,
        _on_message: on_message,
    };

    Ok((renet, transport, answer.client_id))
}

async fn create_session(base_http: &str) -> Result<SessionCreateResponse, WebRtcClientError> {
    let url = format!("{}/api/session/new", base_http.trim_end_matches('/'));
    log::debug!("requesting web session: {url}");
    let response = Request::post(&url)
        .send()
        .await
        .map_err(|err| WebRtcClientError::HttpRequest {
            url: url.clone(),
            detail: err.to_string(),
        })?;

    decode_json_response(response, &url).await
}

async fn post_offer(url: &str, sdp: String) -> Result<SdpHttpAnswerResponse, WebRtcClientError> {
    log::debug!("posting SDP offer to {url}");
    let request = SdpHttpOfferRequest { sdp };
    let response = Request::post(url)
        .json(&request)
        .map_err(|err| WebRtcClientError::HttpRequest {
            url: url.to_string(),
            detail: err.to_string(),
        })?
        .send()
        .await
        .map_err(|err| WebRtcClientError::HttpRequest {
            url: url.to_string(),
            detail: err.to_string(),
        })?;

    decode_json_response(response, url).await
}

async fn decode_json_response<T: serde::de::DeserializeOwned>(
    response: gloo_net::http::Response,
    url: &str,
) -> Result<T, WebRtcClientError> {
    if !response.ok() {
        let status = response.status();
        let body = response.text().await.unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(WebRtcClientError::HttpStatus {
            url: url.to_string(),
            status,
            body,
        });
    }

    response
        .json::<T>()
        .await
        .map_err(|err| WebRtcClientError::HttpDecode {
            url: url.to_string(),
            detail: err.to_string(),
        })
}

async fn await_ice_complete(peer: &RtcPeerConnection) -> Result<(), WebRtcClientError> {
    if peer.ice_gathering_state() == RtcIceGatheringState::Complete {
        log::debug!("ICE gathering already complete");
        return Ok(());
    }

    log::debug!("waiting for ICE gathering to complete...");

    let started_at = js_sys::Date::now();
    let mut previous_state = peer.ice_gathering_state();
    loop {
        let current_state = peer.ice_gathering_state();
        if current_state != previous_state {
            log::debug!(
                "ICE gathering state changed: {} -> {}",
                ice_gather_state_label(previous_state),
                ice_gather_state_label(current_state)
            );
            previous_state = current_state;
        }

        if current_state == RtcIceGatheringState::Complete {
            return Ok(());
        }

        if elapsed_ms(started_at) > ICE_GATHER_TIMEOUT_MS {
            return Err(WebRtcClientError::IceGatherTimeout {
                timeout_ms: ICE_GATHER_TIMEOUT_MS,
            });
        }

        TimeoutFuture::new(STATE_POLL_INTERVAL_MS).await;
    }
}

async fn await_data_channel_open(channel: &RtcDataChannel) -> Result<(), WebRtcClientError> {
    if channel.ready_state() == RtcDataChannelState::Open {
        log::debug!("data channel already open");
        return Ok(());
    }

    log::debug!("waiting for data channel open...");

    let started_at = js_sys::Date::now();
    let mut previous_state = channel.ready_state();
    loop {
        let current_state = channel.ready_state();
        if current_state != previous_state {
            log::debug!(
                "data channel state changed: {} -> {}",
                data_channel_state_label(previous_state),
                data_channel_state_label(current_state)
            );
            previous_state = current_state;
        }

        match current_state {
            RtcDataChannelState::Open => return Ok(()),
            RtcDataChannelState::Closing | RtcDataChannelState::Closed => {
                return Err(WebRtcClientError::DataChannelState {
                    state: data_channel_state_label(current_state),
                });
            }
            RtcDataChannelState::Connecting => {}
            _ => {}
        }

        if elapsed_ms(started_at) > DATA_CHANNEL_OPEN_TIMEOUT_MS {
            return Err(WebRtcClientError::DataChannelOpenTimeout {
                timeout_ms: DATA_CHANNEL_OPEN_TIMEOUT_MS,
            });
        }

        TimeoutFuture::new(STATE_POLL_INTERVAL_MS).await;
    }
}

fn message_bytes(event: &MessageEvent) -> Option<Vec<u8>> {
    let data = event.data();

    if let Ok(buffer) = data.clone().dyn_into::<ArrayBuffer>() {
        return Some(Uint8Array::new(&buffer).to_vec());
    }

    if let Ok(array) = data.dyn_into::<Uint8Array>() {
        return Some(array.to_vec());
    }

    log::debug!("ignoring unsupported datachannel message type");
    None
}

fn browser_now_duration() -> Duration {
    Duration::from_millis(js_sys::Date::now() as u64)
}

fn local_sdp_has_candidates(peer: &RtcPeerConnection) -> bool {
    peer.local_description()
        .map(|desc| desc.sdp().contains("a=candidate:"))
        .unwrap_or(false)
}

fn elapsed_ms(started_at: f64) -> u32 {
    (js_sys::Date::now() - started_at).max(0.0) as u32
}

fn data_channel_state_label(state: RtcDataChannelState) -> &'static str {
    match state {
        RtcDataChannelState::Connecting => "connecting",
        RtcDataChannelState::Open => "open",
        RtcDataChannelState::Closing => "closing",
        RtcDataChannelState::Closed => "closed",
        _ => "unknown",
    }
}

fn ice_gather_state_label(state: RtcIceGatheringState) -> &'static str {
    match state {
        RtcIceGatheringState::New => "new",
        RtcIceGatheringState::Gathering => "gathering",
        RtcIceGatheringState::Complete => "complete",
        _ => "unknown",
    }
}

fn js_error(value: JsValue) -> WebRtcClientError {
    WebRtcClientError::Javascript(value.as_string().unwrap_or_else(|| format!("{value:?}")))
}
