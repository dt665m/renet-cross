# Transport tests and reproduction

Run from the repository root:

```sh
./scripts/check.sh
```

Install the browser compilation target first if missing:

```sh
rustup target add wasm32-unknown-unknown
```

The script checks the library package, not the Bevy demo workspace. Tests need
loopback UDP sockets but no public network, browser, STUN/TURN service, game
assets, wall-clock sleeps, or credentials. Dependencies must already be cached
for offline use. Cargo.lock fixes the dependency resolution; CI checks native
transports on Linux, macOS, and Windows and compiles/lints the wasm library.

## Coverage

| Suite | What it exercises |
| --- | --- |
| `tests/udp_transport.rs` | Real UDP, netcode handshakes, secure and unsecure authentication, Renet messages/fragmentation, fixed-schedule packet faults, duplicate identity rejection, foreign-source rejection, oversized datagrams, timeout |
| `src/server_tests.rs` | Real str0m ICE/DTLS/SCTP + netcode + Renet; bidirectional delivery, disconnect, burst ingest limits, event preservation during send, route and identity isolation, drain resumption |
| `src/udp_io.rs` tests | The production receive scheduler with injected busy/empty/error outcomes; verifies bounded work and IO error handling without assumptions about OS delivery timing |
| `src/web_config.rs` tests | Buffer arithmetic boundaries, bounded inbox eviction, invalid message sizes, option validation |
| Bootstrap unit tests | Expiration immediately before/at deadlines, zero TTL, activation, cleanup, deactivate/reissue; injected time without sleeps |
| SDP/Axum tests | Malformed/duplicate/unknown offers and pending-peer capacity |

The UDP proxy drops every seventh datagram, delays every third by three 10 ms
steps, and duplicates every eleventh. Later packets can overtake delayed ones.
Both directions use the same fixed scheduling rules. Tests assert eventual
ordered, exactly-once reliable delivery and byte-for-byte payload equality,
including fragmented messages. They do not demand reliable delivery from the
unreliable channel. Protocol timeouts have finite simulated-step limits.

str0m tests use a synthetic `Instant` advanced in 10 ms steps. They exercise the
actual transport implementation via its private `update_at` seam. Cryptographic
keys, ephemeral ports, and some ICE internals vary; these are repeatable behavioral
scenarios, not byte-identical packet captures or a deterministic OS simulator.

To isolate a failure:

```sh
cargo test -p renet-cross --all-features --locked --test udp_transport -- --nocapture
cargo test -p renet-cross --all-features --locked server::tests -- --nocapture
cargo test -p renet-cross --all-features --locked web_config::tests
```

## What these checks do not establish

Wasm compilation is not a browser interoperability test. str0m talking to str0m
does not prove Chrome, Firefox, Safari, mobile browser, TURN, NAT rebinding, or
background-tab behavior. CI configuration is not evidence that every OS runner
has already passed. These tests also do not establish server player capacity,
bandwidth efficiency, tail latency, or a production security audit.

Before release, exercise actual supported browsers against the server and record
browser/OS versions, crate lockfile, pump interval, player count, payload sizes,
network RTT/loss/jitter, and disconnect/drop counts. Include a background/foreground
cycle, an inaccessible UDP route, relay-only connectivity, and long reliable
bursts alongside frequent unreliable updates. Keep those deployment tests separate
from the fast local regression suite.
