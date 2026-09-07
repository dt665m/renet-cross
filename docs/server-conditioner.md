# Server packet conditioning

Version 0.6.0 provides server controls through runtime configuration,
without conditioner feature flags.
The published 0.5.0 crate has client controls only. Upgrade to 0.6.0 to use these server APIs.

```rust
use std::time::Duration;
use renet_cross::{conditioner::ConditionerConfig,
    server_conditioner::{ServerConditionerConfig, ServerConditionerHandle}};

let handle = ServerConditionerHandle::new(ServerConditionerConfig {
    packets: ConditionerConfig {
        enabled: true,
        latency: Duration::from_millis(150),
        jitter: Duration::from_millis(10),
        packet_loss: 0.01,
        ..Default::default()
    },
    ..Default::default()
})?;
let config = renet_cross::ServerTransportConfig { conditioner: Some(handle.clone()) };
let transport = renet_cross::MixedTransportBuilder::new(protocol_id)
    .transport_config(config)
    .build()?;
```

For standalone UDP or WebRTC servers, pass `ServerTransportConfig` to
`new_with_config(server_config, socket, config)`. Construction installs the
controls before native netcode handshakes.
The policy also applies to clients that arrive later. `latency` is added in each
direction: 150 ms adds approximately 300 ms to each client's existing RTT. No
baseline is required. Client and server conditioning add together if both are on.
CLI percentages should be divided by 100 before assigning `packet_loss`.

Keep advancing Renet and transport updates and sending packets normally. The
conditioner delays encrypted netcode packets, so acknowledgement RTT, reliable
retries, handshakes, and keepalives experience the impairment. In WebRTC the
boundary is the data channel: str0m continues pumping ICE/DTLS/SCTP normally.
This does not simulate their establishment, wire MTU, congestion, or OS queues.

Use `handle.config()` and `handle.configure(config)` for runtime changes;
`handle.outage(duration)` starts a brief all-clients outage. Configuration changes
clear queued packets rather than releasing old traffic in a burst. Disabling
uses `config.packets.enabled = false`. The headless handle can be retained in an
application resource for built-in Bevy UI; no Bevy dependency is needed by the
server controls. The existing client RTT-baseline panel is not a server baseline
estimator: clients have different baseline RTTs.

`handle.stats()` reports aggregate `packets` (incoming/outgoing counters), live
`peers`, `peer_limit_drops`, and `expired_peers`. `handle.per_peer_stats()` returns
counters keyed by UDP address or WebRTC signaling client ID. Queue packet and byte
limits are independent for each direction of each peer. `max_peers` bounds the
total table, including untrusted UDP sources awaiting a handshake; new entries at
capacity are dropped. `idle_timeout` bounds abandoned entries. A controller may
be shared by the UDP and WebRTC halves of one mixed server, with distinct peer
identities and independent schedulers.

Peer removal, transport replacement, and disconnect-all discard their queues.
UDP terminal disconnect notifications are best-effort immediate sends after
queue cleanup, so teardown does not retain traffic for a departed session.
WebRTC teardown likewise releases the peer instead of retaining delayed traffic.

The private `packet_io` module applies runtime policy. With no handle configured,
packets pass straight through; transport loops contain no conditioner feature branching.
