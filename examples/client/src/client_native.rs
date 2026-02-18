use renet::RenetClient;

pub type NativeUdpTransport = renet_server::UdpNetcodeClientTransport;

pub fn connect(
    base_http: &str,
    protocol_id: u64,
) -> Result<(RenetClient, NativeUdpTransport, u64), String> {
    renet_server::connect_via_session_http_blocking(
        base_http,
        protocol_id,
        renet_server::NativeConnectOptions::default(),
    )
    .map_err(|err| err.to_string())
}
