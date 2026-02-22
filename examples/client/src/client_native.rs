use renet::RenetClient;

pub type NativeUdpTransport = renet_cross::UdpNetcodeClientTransport;

pub fn connect(
    base_http: &str,
    protocol_id: u64,
) -> Result<(RenetClient, NativeUdpTransport, u64), String> {
    renet_cross::connect_via_session_http_blocking(
        base_http,
        protocol_id,
        renet_cross::NativeConnectOptions::default(),
    )
    .map_err(|err| err.to_string())
}
