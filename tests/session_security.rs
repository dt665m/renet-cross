#![cfg(not(target_arch = "wasm32"))]
use renet::{ConnectionConfig, RenetClient, RenetServer};
use renet_cross::*;
use std::{
    net::UdpSocket,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
const PROTOCOL: u64 = 77;
const KEY: [u8; 32] = [42; 32];
fn now() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
}
fn request() -> SessionCreateRequest {
    SessionCreateRequest {
        protocol_id: Some(PROTOCOL),
        service: "test-service".into(),
        match_id: "match-7".into(),
        credential: "ticket-a".into(),
        require_secure: true,
    }
}
struct TicketPolicy;
impl SessionAdmission for TicketPolicy {
    fn admit(
        &self,
        request: &SessionCreateRequest,
        now: Duration,
    ) -> Result<SessionGrant, BootstrapAuthError> {
        let replay_key = match request.credential.as_str() {
            "ticket-a" => [1; 32],
            "ticket-b" => [2; 32],
            _ => return Err(BootstrapAuthError::InvalidGrant),
        };
        Ok(SessionGrant {
            protocol_id: PROTOCOL,
            service: "test-service".into(),
            match_id: "match-7".into(),
            expires_at: now.as_secs() + 60,
            replay_key,
            application: vec![9, 7],
        })
    }
}
type Service = BootstrapService<MonotonicClientIdAllocator, SecureSessionAuthPolicy<TicketPolicy>>;
fn service(udp: std::net::SocketAddr, webrtc: std::net::SocketAddr) -> Service {
    BootstrapService::new(
        BootstrapConfig {
            public_udp_addr: udp,
            public_webrtc_addr: webrtc,
            ..Default::default()
        },
        MonotonicClientIdAllocator::new(10),
        SecureSessionAuthPolicy::new(TicketPolicy, KEY),
    )
}
fn options() -> NativeConnectOptions {
    NativeConnectOptions {
        session_request: request(),
        require_secure: true,
        ..Default::default()
    }
}

#[test]
fn issuance_rejects_invalid_expired_wrong_binding_and_replayed_tickets() {
    let service = service(
        "127.0.0.1:5000".parse().unwrap(),
        "127.0.0.1:5001".parse().unwrap(),
    );
    assert!(service.create_session().is_err());
    let mut wrong = request();
    wrong.credential = "invalid".into();
    assert!(service.create_session_with_request(&wrong).is_err());
    wrong = request();
    wrong.protocol_id = Some(PROTOCOL + 1);
    assert!(service.create_session_with_request(&wrong).is_err());
    wrong = request();
    wrong.match_id = "other-match".into();
    assert!(service.create_session_with_request(&wrong).is_err());
    wrong = request();
    wrong.service = "other-service".into();
    assert!(service.create_session_with_request(&wrong).is_err());
    let session = service.create_session_with_request(&request()).unwrap();
    assert!(matches!(
        service.create_session_with_request(&request()),
        Err(BootstrapError::Auth(BootstrapAuthError::Replay))
    ));
    service.on_client_disconnected(session.client_id);
    assert!(matches!(
        service.create_session_with_request(&request()),
        Err(BootstrapError::Auth(BootstrapAuthError::Replay))
    ));
    struct Expired;
    impl SessionAdmission for Expired {
        fn admit(
            &self,
            request: &SessionCreateRequest,
            now: Duration,
        ) -> Result<SessionGrant, BootstrapAuthError> {
            let mut grant = TicketPolicy.admit(request, now)?;
            grant.expires_at = now.as_secs();
            Ok(grant)
        }
    }
    let expired = BootstrapService::new(
        BootstrapConfig::default(),
        MonotonicClientIdAllocator::new(1),
        SecureSessionAuthPolicy::new(Expired, KEY),
    );
    assert!(matches!(
        expired.create_session_with_request(&request()),
        Err(BootstrapError::Auth(BootstrapAuthError::InvalidGrant))
    ));
}

#[test]
fn malformed_credentials_are_rejected_before_policy_and_dev_cannot_ignore_auth() {
    struct NeverCalled;
    impl SessionAdmission for NeverCalled {
        fn admit(
            &self,
            _: &SessionCreateRequest,
            _: Duration,
        ) -> Result<SessionGrant, BootstrapAuthError> {
            panic!("oversized request reached verifier")
        }
    }
    let secure = BootstrapService::new(
        BootstrapConfig::default(),
        MonotonicClientIdAllocator::new(1),
        SecureSessionAuthPolicy::new(NeverCalled, KEY),
    );
    let mut large = request();
    large.credential = "a".repeat(MAX_SESSION_CREDENTIAL_BYTES + 1);
    assert!(matches!(
        secure.create_session_with_request(&large),
        Err(BootstrapError::Auth(BootstrapAuthError::InvalidRequest))
    ));
    let dev = BootstrapService::new(
        BootstrapConfig::default(),
        MonotonicClientIdAllocator::new(1),
        UnsecureDevAuthPolicy,
    );
    assert!(dev.create_session().unwrap().security.is_none());
    assert!(matches!(
        dev.create_session_with_request(&request()),
        Err(BootstrapError::Auth(BootstrapAuthError::AdmissionRequired))
    ));
}

#[test]
fn secure_helpers_bind_both_endpoint_tokens_client_protocol_and_expiry() {
    let udp = "127.0.0.1:5000".parse().unwrap();
    let web = "127.0.0.1:5001".parse().unwrap();
    let service = service(udp, web);
    let session = service.create_session_with_request(&request()).unwrap();
    let authentication = |session: &SessionCreateResponse, protocol, addr, transport, time| {
        session.authentication(protocol, addr, transport, time, true, &request())
    };
    assert!(matches!(
        authentication(&session, PROTOCOL, udp, SessionTransport::Udp, now()).unwrap(),
        ClientAuthentication::Secure { .. }
    ));
    assert!(matches!(
        authentication(&session, PROTOCOL, web, SessionTransport::WebRtc, now()).unwrap(),
        ClientAuthentication::Secure { .. }
    ));
    assert!(authentication(&session, PROTOCOL, web, SessionTransport::Udp, now()).is_err());
    assert!(authentication(&session, PROTOCOL + 1, udp, SessionTransport::Udp, now()).is_err());
    assert!(
        authentication(
            &session,
            PROTOCOL,
            udp,
            SessionTransport::Udp,
            Duration::from_secs(session.security.as_ref().unwrap().expires_at)
        )
        .is_err()
    );
    let mut modified = session.clone();
    modified.client_id += 1;
    assert!(authentication(&modified, PROTOCOL, udp, SessionTransport::Udp, now()).is_err());
    modified = session.clone();
    modified.security.as_mut().unwrap().udp_connect_token = "!malformed!".into();
    assert!(authentication(&modified, PROTOCOL, udp, SessionTransport::Udp, now()).is_err());
    modified = session.clone();
    modified.security.as_mut().unwrap().udp_connect_token =
        "a".repeat(MAX_ENCODED_CONNECT_TOKEN_BYTES + 1);
    assert!(authentication(&modified, PROTOCOL, udp, SessionTransport::Udp, now()).is_err());
    modified = session.clone();
    modified.security = None;
    assert!(matches!(
        authentication(&modified, PROTOCOL, udp, SessionTransport::Udp, now()),
        Err(SessionSecurityError::SecureRequired)
    ));
    // Supplying a credential itself forbids fallback, even if the bool is false.
    assert!(
        modified
            .authentication(
                PROTOCOL,
                udp,
                SessionTransport::Udp,
                now(),
                false,
                &request()
            )
            .is_err()
    );
    let debug = format!("{session:?} {:?}", request());
    assert!(!debug.contains("ticket-a"));
    assert!(!debug.contains(session.session_token.as_ref().unwrap()));
    assert!(!debug.contains(&session.security.as_ref().unwrap().udp_connect_token));
}

#[test]
fn real_secure_udp_bootstrap_activates_once_with_authenticated_grant() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let service = service(addr, "127.0.0.1:5001".parse().unwrap());
    let session = service.create_session_with_request(&request()).unwrap();
    let id = session.client_id;
    let mut server_transport = UdpNetcodeServerTransport::new(
        ServerConfig {
            current_time: now(),
            max_clients: 4,
            protocol_id: PROTOCOL,
            public_addresses: vec![addr],
            authentication: ServerAuthentication::Secure { private_key: KEY },
        },
        socket,
    )
    .unwrap();
    let mut server = RenetServer::new(ConnectionConfig::default());
    let (mut client, mut client_transport, _) =
        connect_from_session(session, PROTOCOL, options()).unwrap();
    for _ in 0..500 {
        let dt = Duration::from_millis(10);
        client.update(dt);
        server.update(dt);
        client_transport.update(dt, &mut client).unwrap();
        server_transport.update(dt, &mut server).unwrap();
        server_transport.send_packets(&mut server);
        if client.is_connected() && server.is_connected(id) {
            break;
        }
    }
    assert!(client.is_connected());
    assert!(server.is_connected(id));
    assert!(matches!(
        service.on_client_connected(id),
        Err(BootstrapError::Auth(BootstrapAuthError::AdmissionRequired))
    ));
    let data = server_transport.user_data(id).unwrap();
    let mut wrong = data;
    wrong[1] ^= 1;
    assert!(
        service
            .on_client_connected_with_user_data(id, &wrong)
            .is_err()
    );
    service
        .on_client_connected_with_user_data(id, &data)
        .unwrap();
    assert!(
        service
            .on_client_connected_with_user_data(id, &data)
            .is_err()
    );
    assert_eq!(
        service.session_grant(id).unwrap().unwrap().application,
        vec![9, 7]
    );
    service.on_client_disconnected(id);
    assert!(
        service
            .on_client_connected_with_user_data(id, &data)
            .is_err()
    );
}

#[test]
fn secure_udp_rejects_wrong_protocol_unsecure_corrupt_and_expired_tokens() {
    for case in 0..4 {
        let unsecure = case == 1;
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let service = service(addr, "127.0.0.1:5001".parse().unwrap());
        let session = service.create_session_with_request(&request()).unwrap();
        let id = session.client_id;
        let mut auth = if unsecure {
            ClientAuthentication::Unsecure {
                protocol_id: PROTOCOL,
                client_id: id,
                server_addr: addr,
                user_data: None,
            }
        } else {
            session
                .authentication(
                    PROTOCOL,
                    addr,
                    SessionTransport::Udp,
                    now(),
                    true,
                    &request(),
                )
                .unwrap()
        };
        if case == 2 {
            if let ClientAuthentication::Secure { connect_token } = &mut auth {
                connect_token.private_data[0] ^= 1;
            }
        }
        let server_now = if case == 3 {
            now() + Duration::from_secs(61)
        } else {
            now()
        };
        let mut transport = UdpNetcodeServerTransport::new(
            ServerConfig {
                current_time: server_now,
                max_clients: 4,
                protocol_id: if case == 0 { PROTOCOL + 1 } else { PROTOCOL },
                public_addresses: vec![addr],
                authentication: ServerAuthentication::Secure { private_key: KEY },
            },
            socket,
        )
        .unwrap();
        let mut server = RenetServer::new(ConnectionConfig::default());
        let mut client = RenetClient::new(ConnectionConfig::default());
        let mut client_transport =
            UdpNetcodeClientTransport::new(now(), auth, UdpSocket::bind("127.0.0.1:0").unwrap())
                .unwrap();
        for _ in 0..200 {
            let dt = Duration::from_millis(10);
            server.update(dt);
            client.update(dt);
            let _ = client_transport.update(dt, &mut client);
            transport.update(dt, &mut server).unwrap();
            transport.send_packets(&mut server);
        }
        assert!(!server.is_connected(id));
        assert!(!client.is_connected());
    }
}

#[test]
fn registry_limits_pending_active_and_replay_history() {
    let limits = BootstrapLimits {
        pending_sessions: 1,
        active_sessions: 1,
        replay_grants: 1,
    };
    let service = BootstrapService::with_limits(
        BootstrapConfig::default(),
        MonotonicClientIdAllocator::new(1),
        SecureSessionAuthPolicy::new(TicketPolicy, KEY),
        limits,
    )
    .unwrap();
    let first = service.create_session_with_request(&request()).unwrap();
    let mut next = request();
    next.credential = "ticket-b".into();
    assert!(matches!(
        service.create_session_with_request(&next),
        Err(BootstrapError::Capacity)
    ));
    let data = service
        .session_grant(first.client_id)
        .unwrap()
        .unwrap()
        .user_data()
        .unwrap();
    service
        .on_client_connected_with_user_data(first.client_id, &data)
        .unwrap();
    assert!(matches!(
        service.create_session_with_request(&next),
        Err(BootstrapError::Capacity)
    ));
    service.on_client_disconnected(first.client_id);
    // Removing a live session does not remove the admission replay fence.
    assert!(matches!(
        service.create_session_with_request(&next),
        Err(BootstrapError::Capacity)
    ));
    let mut registry =
        InMemorySessionRegistry::with_limits(Duration::from_secs(60), limits).unwrap();
    registry.try_issue(1).unwrap();
    assert!(registry.try_issue(2).is_err());
    assert!(registry.activate(1));
    assert!(registry.try_issue(2).is_err());
    assert_eq!(registry.counts(), (0, 1, 0));
}

#[test]
fn sdp_offer_token_is_bound_to_one_pending_session_and_claim() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let service = service("127.0.0.1:5000".parse().unwrap(), addr);
    let a = service.create_session_with_request(&request()).unwrap();
    let mut next = request();
    next.credential = "ticket-b".into();
    let b = service.create_session_with_request(&next).unwrap();
    let mut transport = WebRtcNetcodeServerTransport::new(
        ServerConfig {
            current_time: now(),
            max_clients: 4,
            protocol_id: PROTOCOL,
            public_addresses: vec![addr],
            authentication: ServerAuthentication::Secure { private_key: KEY },
        },
        socket,
    )
    .unwrap();
    let offer = |token| SdpHttpOfferRequest {
        sdp: "not sdp".into(),
        session_token: token,
    };
    assert!(matches!(
        service.accept_offer(
            &mut transport,
            a.client_id,
            offer(b.session_token),
            SdpHttpHookConfig::new(addr)
        ),
        Err(BootstrapError::Auth(
            BootstrapAuthError::InvalidToken { .. }
        ))
    ));
    assert!(matches!(
        service.accept_offer(
            &mut transport,
            a.client_id,
            offer(a.session_token.clone()),
            SdpHttpHookConfig::new(addr)
        ),
        Err(BootstrapError::Hook(_))
    ));
    assert!(matches!(
        service.accept_offer(
            &mut transport,
            a.client_id,
            offer(a.session_token),
            SdpHttpHookConfig::new(addr)
        ),
        Err(BootstrapError::UnknownSession { .. })
    ));
    assert_eq!(transport.peer_count(), 0);
}

#[cfg(any(feature = "native-sync", feature = "native-async"))]
fn http_once(
    session: SessionCreateResponse,
    status: u16,
    body_override: Option<String>,
) -> (String, std::thread::JoinHandle<SessionCreateRequest>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut scratch = [0; 1024];
        let header_end = loop {
            let n = socket.read(&mut scratch).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&scratch[..n]);
            assert!(bytes.len() <= MAX_SESSION_REQUEST_BYTES + 4096);
            if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
        assert!(headers.starts_with("POST /api/session/new "));
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse().unwrap())
            })
            .unwrap();
        assert!(length <= MAX_SESSION_REQUEST_BYTES);
        while bytes.len() - header_end < length {
            let n = socket.read(&mut scratch).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&scratch[..n]);
        }
        let request = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        let body = body_override.unwrap_or_else(|| serde_json::to_string(&session).unwrap());
        let response = format!(
            "HTTP/1.1 {status} Response\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes());
        request
    });
    (url, thread)
}
#[cfg(feature = "native-sync")]
#[test]
fn blocking_http_forwards_credentials_and_redacts_error_bodies() {
    let service = service(
        "127.0.0.1:5000".parse().unwrap(),
        "127.0.0.1:5001".parse().unwrap(),
    );
    let session = service.create_session_with_request(&request()).unwrap();
    let (url, thread) = http_once(session.clone(), 200, None);
    let (_, _, id) = connect_via_session_http_blocking(&url, PROTOCOL, options()).unwrap();
    assert_eq!(id, session.client_id);
    let observed = thread.join().unwrap();
    assert_eq!(observed.credential, "ticket-a");
    assert!(observed.require_secure);
    assert_eq!(observed.protocol_id, Some(PROTOCOL));
    let (url, thread) = http_once(
        session.clone(),
        403,
        Some("SECRET-token-should-not-be-logged".into()),
    );
    let error = connect_via_session_http_blocking(&url, PROTOCOL, options()).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("SECRET"));
    thread.join().unwrap();
    let (url, thread) = http_once(
        session,
        200,
        Some("x".repeat(MAX_SESSION_RESPONSE_BYTES + 1)),
    );
    assert!(matches!(
        connect_via_session_http_blocking(&url, PROTOCOL, options()),
        Err(NativeClientError::BodyTooLarge)
    ));
    thread.join().unwrap();
}
#[cfg(feature = "native-async")]
#[tokio::test]
async fn async_http_forwards_credentials() {
    let service = service(
        "127.0.0.1:5000".parse().unwrap(),
        "127.0.0.1:5001".parse().unwrap(),
    );
    let session = service.create_session_with_request(&request()).unwrap();
    let (url, thread) = http_once(session.clone(), 200, None);
    let (_, _, id) = connect_via_session_http_async(&url, PROTOCOL, options())
        .await
        .unwrap();
    assert_eq!(id, session.client_id);
    let observed = thread.join().unwrap();
    assert_eq!(observed.credential, "ticket-a");
    assert!(observed.require_secure);
    assert_eq!(observed.service, "test-service");
}

#[test]
fn default_transport_preserves_authenticated_application_grant() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let service = BootstrapService::new(
        BootstrapConfig {
            public_udp_addr: address,
            public_webrtc_addr: address,
            ..Default::default()
        },
        MonotonicClientIdAllocator::new(10),
        SecureSessionAuthPolicy::new(TicketPolicy, KEY),
    );
    let requested = request();
    let response = service.create_session_with_request(&requested).unwrap();
    let options = NativeConnectOptions {
        session_request: requested.clone(),
        require_secure: true,
        ..Default::default()
    };
    let mut altered = response.clone();
    altered.security.as_mut().unwrap().protocol_id ^= 1;
    assert!(connect_from_session(altered, PROTOCOL, options.clone()).is_err());
    let (mut client, mut transport, id) =
        connect_from_session(response, PROTOCOL, options).unwrap();
    let mut server = RenetServer::new(ConnectionConfig::default());
    let mut server_transport = UdpNetcodeServerTransport::new(
        ServerConfig {
            current_time: now(),
            max_clients: 8,
            protocol_id: PROTOCOL,
            public_addresses: vec![address],
            authentication: ServerAuthentication::Secure { private_key: KEY },
        },
        socket,
    )
    .unwrap();
    for _ in 0..100 {
        client.update(Duration::from_millis(10));
        server.update(Duration::from_millis(10));
        transport
            .update(Duration::from_millis(10), &mut client)
            .unwrap();
        transport.send_packets(&mut client).unwrap();
        server_transport
            .update(Duration::from_millis(10), &mut server)
            .unwrap();
        server_transport.send_packets(&mut server);
        if client.is_connected() {
            break;
        }
    }
    assert!(client.is_connected());
    let userdata = server_transport.user_data(id).unwrap();
    assert_eq!(&userdata[248..], &[0; 8]);
    let mut altered_userdata = userdata;
    altered_userdata[1] ^= 1;
    assert!(
        service
            .on_client_connected_with_user_data(id, &altered_userdata)
            .is_err()
    );
    service
        .on_client_connected_with_user_data(id, &userdata)
        .unwrap();
}

#[test]
fn full_application_grant_capacity_and_obsolete_packet_options() {
    let grant = SessionGrant {
        protocol_id: PROTOCOL,
        service: "s".repeat(MAX_SERVICE_BYTES),
        match_id: "m".repeat(MAX_MATCH_BYTES),
        expires_at: now().as_secs() + 60,
        replay_key: [1; 32],
        application: vec![0xa5; MAX_GRANT_BYTES],
    };
    let data = grant.user_data().unwrap();
    assert_eq!(&data[116..244], &[0xa5; MAX_GRANT_BYTES]);
    assert_eq!(&data[244..], &[0; 12]);
    let mut obsolete = serde_json::to_value(request()).unwrap();
    obsolete["packet_profile"] =
        serde_json::json!({"kind": "bounded_v1", "max_packet_bytes": 1127});
    assert!(serde_json::from_value::<SessionCreateRequest>(obsolete).is_err());
}
