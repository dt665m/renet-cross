# Hybrid Bootstrap (Axum + Authoritative Mixed Transport)

This bootstrap demonstrates one authoritative game server that accepts:
- native UDP clients
- browser WebRTC clients

The game loop and authority are shared, with one `RenetServer` and one `MixedServerTransport` (`udp + webrtc`).

## Architecture

```mermaid
flowchart LR
  subgraph Browser
    W["Bevy wasm client"]
    D["WebRTC DataChannel"]
  end

  subgraph Native
    N["Bevy native client"]
    U["UDP socket"]
  end

  subgraph Server
    A["Axum API"]
    M["MixedServerTransport"]
    R["RenetServer"]
    S["Authoritative sim (20 Hz)"]
  end

  W -->|"POST /api/session/new"| A
  N -->|"POST /api/session/new"| A
  W -->|"POST /api/webrtc/offer/{client_id}"| A

  D --> M
  U --> M
  M --> R
  R --> S
  S --> R
  R --> M
```

## HTTP and Signaling Flow

1. `POST /api/session/new`
- returns `SessionCreateResponse { client_id, udp_addr, webrtc_addr, webrtc_offer_url }`

2. Browser only: `POST /api/webrtc/offer/{client_id}`
- request: `{ "sdp": "...offer..." }`
- response: `{ "client_id": <u64>, "sdp": "...answer..." }`

3. Native client
- uses `udp_addr` with unsecure netcode auth for local/dev bootstrap

## Tick Order (Authoritative)

Per tick (`20 Hz`):
1. `server.update(dt)`
2. `transport.update(dt, &mut server)`
3. process connect/disconnect and input messages
4. `sim.step()`
5. broadcast `WorldDelta` (`DefaultChannel::Unreliable`)
6. `transport.send_packets(&mut server)`

## Startup Steps

### 1. Run server

```bash
just -f examples/justfile server
```

Optional CLI flags (also supported via env fallback):
- `--http-bind` (`NET_HTTP_BIND`, default `0.0.0.0:8080`)
- `--udp-bind` (`NET_UDP_BIND`, default `0.0.0.0:5000`)
- `--webrtc-bind` (`NET_WEBRTC_BIND`, default `0.0.0.0:5001`)
- `--public-http-base` (`NET_PUBLIC_HTTP_BASE`, default `http://127.0.0.1:8080`)
- `--public-udp-addr` (`NET_PUBLIC_UDP_ADDR`, default `127.0.0.1:5000`)
- `--public-webrtc-addr` (`NET_PUBLIC_WEBRTC_ADDR`, default `127.0.0.1:5001`)
- `--client-dist` (`NET_CLIENT_DIST`, default `examples/client/dist`)

### 2. Run native client

```bash
just -f examples/justfile desktop-client
```

Optional client flag:
- `--http-base` (`NET_HTTP_BASE`, default `http://127.0.0.1:8080`)

### 3. Run wasm client (Trunk)

```bash
cd examples/client
trunk serve --open
```

The wasm client posts to the current page origin by default (or fallback `http://127.0.0.1:8080`).

## Why `client_id` Exists

`client_id` is connection identity, not transport identity.

- transport decides how bytes move (`UDP` or `WebRTC`)
- `client_id` decides which logical player/connection those bytes belong to in `RenetServer`

Both transports can coexist because both eventually map incoming/outgoing packets to the same authoritative `client_id` model.

This bootstrap uses `MonotonicClientIdAllocator` (in-memory, monotonic) as a dev default.

## Failure Mapping

Current API behavior:
- duplicate WebRTC offer id -> `409 Conflict`
- invalid SDP offer -> `400 Bad Request`
- ICE/RTC negotiation errors -> `422 Unprocessable Entity`

Unknown/expired session ids return `404` in the server example.

## Future Hardening (Production Path)

Move from in-memory monotonic IDs to signed, time-bounded issuance:
1. mint signed session/bootstrap tokens from trusted auth service
2. bind token to `client_id`, audience, expiry, and optional device/account context
3. verify token server-side before creating transport peer
4. replace unsecure connect auth with secure token issuance and key rotation

This keeps transport-agnostic identity while making bootstrap secure and replay-resistant.
