# Packet sizes and egress budgets

Both endpoints use stock Renet 2.0.0 packetization through `RenetClient::new`
and `RenetServer::new`. Configure channel reliability, priority and memory through
Renet's `ConnectionConfig`; the adapter adds no packet format or size negotiation.
The retained renetcode handshake correction is documented in the
[development dependency guidance](../README.md#development-dependency-corrections).

## Native UDP and logical WebRTC messages

Renet uses 1,200-byte slice payloads and a 1,300-byte serialized packet bound.
Netcode encryption adds at most 25 bytes: one prefix byte, up to eight sequence
bytes, and a 16-byte authentication tag. The resulting data-packet allowances are:

| Layer | Maximum bytes |
| --- | ---: |
| Serialized Renet packet | 1300 |
| Encrypted netcode payload | 1325 |
| Base IPv6 header + UDP header + encrypted payload | 1373 |

The UDP transport tests measure real socket payloads under reliable slicing,
aggregation, loss, duplication and reordering. They explicitly exercise packets
above the former 1,200-byte IP envelope. A 1,100-byte application frame is a small
Renet message; that application limit does not cap the aggregated network packet.
Larger reliable messages are fragmented, acknowledged and retried by Renet.

WebRTC carries each encrypted netcode packet as one logical DataChannel message.
SCTP may fragment it, and browser `RTCDataChannel` does not expose physical UDP
MTU control. Logical packet limits therefore do not promise a physical WebRTC
datagram size. The adapter retains its separate bounded backend output queues.

Secure admission still binds protocol, service, match, expiry, replay key and
application grant to authenticated netcode user data. Removed packet-profile
fields are rejected by the strict session request decoder. Both endpoints must
use the same game protocol and channel configuration.

## Validation

```sh
cargo test --locked -p renet-cross --all-features
cargo check --locked -p renet-cross --target wasm32-unknown-unknown
```

Tests cover real UDP and ICE/DTLS/SCTP peers, exact reliable message delivery,
packet lengths, secure grant enforcement and recovery when the first handshake
KeepAlive is lost while application payloads are sent continuously.

## Transport egress rate limits

Packet size and byte rate are independent policies. Every UDP and WebRTC client
and server transport exposes `set_egress_limit(Some(config))`. Configure before
its first update to include the netcode handshake. The default is unpaced.
Construction validates the rate, burst, and reserved capacity; installation
rejects an accounting basis that does not match the backend without changing the
existing policy:

```rust
use renet_cross::{EgressBasis, EgressConfig};
let native = EgressConfig::new(EgressBasis::NativeUdpIp, 60_000, 6_000, 512)?;
udp_transport.set_egress_limit(Some(native))?;
let logical = EgressConfig::new(EgressBasis::EncryptedLogical, 60_000, 6_000, 512)?;
webrtc_transport.set_egress_limit(Some(logical))?;
```

Native UDP charges every final socket send attempt for encrypted datagram bytes
plus 28 bytes for IPv4 or 48 for IPv6 (IPv4-mapped destinations use IPv4). This
includes Renet headers, acknowledgements, retransmissions and all netcode
handshake, keepalive and disconnect packets. Charging happens after packet
conditioning, so released delays and duplicates also consume capacity. Counts
represent successfully accepted socket sends, not remote delivery; IP options,
IPv6 extension headers, link-layer framing, and tunneling overhead are excluded.

WebRTC charges encrypted netcode messages accepted by the data channel, including
Renet retries and netcode control messages. ICE, DTLS, SCTP, TURN, IP, and any
browser-managed retransmissions are excluded. Its basis is always
`EncryptedLogical`, and `native_udp_ip_bytes` remains zero. This is a logical
message rate limit and makes no physical WebRTC bandwidth guarantee.

Each destination has a token bucket replenished by monotonic wall time. Passing
additional simulation time to `update` does not replenish it. A connection may
send at most `burst_bytes + bytes_per_second * elapsed_seconds` charged bytes
from initialization; intervals after saturation may include one burst. The
burst is capped even after long inactivity. Data cannot spend
`control_reserve_bytes`, but control still consumes the same total bucket, so
there is no unlimited handshake or disconnect bypass. Budget construction
requires a positive bounded rate, a burst of 2048–67108864 bytes, and reserve of
at least 256 bytes while leaving at least 1448 bytes for data.

Pacing adds no packet queue. A packet that cannot fit current credit is dropped;
Renet retains reliable message state and retransmits it normally. Unreliable
traffic may be lost. Backend backpressure also drops rather than accumulating
stale encrypted packets. Tokens consumed by failed backend sends are not
refunded, so successful sends remain within the ceiling. Low limits can delay
reliable delivery or cause normal connection timeouts.

`egress_stats()` returns cumulative successful packet, encrypted logical byte,
native UDP/IP byte and control packet counts, plus pacing, packet-cap and backend
drop counts. `deferred_packets` is zero. Statistics include their explicit basis
and current config. Servers expose `peer_egress_stats(client_id)` for tracked
peers; aggregate counters remain available after disconnect. Tracking retains
at most 4096 destination identities, retaining credit across reconnects. Further
identities share one bounded overflow bucket, reported by
`peer_capacity_packets`; they do not allocate a per-peer entry. Reconfiguration
preserves counters and never restores spent credit. Enabling pacing after an
already-used unpaced destination starts that destination with no credit.

Validation includes real IPv4 and IPv6 socket accounting, deterministic bucket
bounds/control reserve/clock saturation/peer churn tests, a real secure UDP
bidirectional reliable burst under loss, duplication, reordering and pacing,
and a real str0m WebRTC logical pacing test. The UDP stress compares final socket
receives with transport counters and checks the wall-time rate envelope while
reliable messages recover from pacing drops.
