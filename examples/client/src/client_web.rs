use renet::RenetClient;
pub type WebRtcNetcodeTransport = renet_server::WebRtcNetcodeClientTransport;

const WEB_WEBRTC_ADDR_OVERRIDE: Option<&str> = option_env!("NET_WEB_WEBRTC_ADDR");

pub async fn connect(base_http: &str, protocol_id: u64) -> Result<(RenetClient, WebRtcNetcodeTransport, u64), String> {
    renet_server::connect_via_sdp_http_with_overrides(base_http, protocol_id, WEB_WEBRTC_ADDR_OVERRIDE)
        .await
        .map_err(|err| err.to_string())
}
