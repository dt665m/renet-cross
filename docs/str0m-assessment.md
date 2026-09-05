# str0m assessment for renet-cross

Assessed against the resolved str0m 0.23.1 source and this repository's pump.

## Recommendation

Keep str0m for this architecture. Its caller-driven, synchronous design fits a
shared UDP socket and explicit game-server scheduling. It also lets tests control
time and packet delivery without a browser. Its documentation explicitly requires
draining output after mutations and delegates socket/interface/relay management
to the application. These are essential integration obligations, not optional
performance tuning. [Upstream documentation](https://github.com/algesten/str0m)

"Lightweight" describes the lack of an imposed runtime, internal task model, or
RTCPeerConnection-style wrapper. It does not mean this is a small UDP messaging
protocol: the browser path still carries Renet/netcode over SCTP/DTLS/ICE/UDP,
with congestion control and buffering. Native UDP avoids those WebRTC layers.
[WebRTC data transport architecture](https://www.rfc-editor.org/info/rfc8831/)

## Concrete integration faults corrected

- Receive batching mutated a peer repeatedly before fully draining its outputs.
  Receive, write, and timeout paths now drain between mutations. Exhausting a
  drain budget defers further mutations until the drain reaches a timeout.
- The send flush discarded events, including incoming channel data. All output
  paths now preserve and dispatch events through the same processing path.
- The old timeout pump could advance time while output remained pending. The
  pump records returned deadlines and only supplies due timeout inputs.
- Source caching could bypass str0m demultiplexing, while pinning the first
  observed address blocked ICE candidate changes. Cached routes are now checked
  by `Rtc::accepts`, with an ICE-aware fallback.
- Signaling and netcode identity could disagree. They must now match; explicit
  ownership also prevents WebRTC disconnects from deleting a native connection.
- Peer replacement/removal could leave stale netcode occupancy or queued results;
  disconnect-all omitted half-open negotiations. Lifecycle regressions now cover
  these paths. The SDP helper counts negotiations against backend capacity.

See [the test guide](testing.md) for evidence and reproduction commands. The
tests include real mixed UDP/WebRTC clients on the same RenetServer.

## Remaining costs and limits

The receive policy caps global work at 256 datagrams and per-peer ingress at 24
datagrams per update. This limits work but is not a fair scheduler: queued traffic
from one sender can still consume the socket's global budget. Excess per-peer
traffic is dropped, and reliable Renet traffic must recover. Appropriate limits
depend on pump cadence, message sizes, and player count. No capacity or latency
benchmark currently justifies the builder's 512-clients-per-backend default.

Established source lookups are cached, but fallback routing scans peers. Unknown
traffic can therefore cost O(peer count) per received datagram. A credential index,
fair admission policy, or deadline heap should be introduced only with profiling
and tests around ICE transitions; a source-address-only shortcut is incorrect.

Malformed SCTP/receive-queue errors are classified partly by message text because
of the current error surface. That is brittle across dependency upgrades. Keep the
real-peer regressions and re-check classifications when changing str0m versions.

Network pumping should use elapsed real time independently of simulation time
scaling. Calling update only at 20 or 30 Hz introduces scheduling delay and late
timer service. A more frequent pump can coexist with a slower fixed simulation;
the library still requires callers to schedule it. If modifying `Rtc` directly
through low-level accessors, callers must observe str0m's drain contract too.

The browser now exposes configurable ICE servers and bounded local queues. That
does not provide a deployed TURN service, server-side TURN allocation, universal
NAT traversal, or a fallback through a TCP-only network. The server's advertised
candidate must be reachable and correctly mapped to its bound UDP socket.

The helper bootstrap still uses development netcode authentication. Signaling
tokens and TURN credentials serve different purposes from netcode connect tokens.
Native secure-token transport is covered by tests, but end-to-end secure browser
token issuance/validation remains unfinished. Admission rate limiting and bounds
for the bootstrap session registry also remain deployment/library follow-up work.

## Next measurements

Before changing transport dependencies, measure CPU and allocation cost per peer,
outbound queue depth, ingress drops, actual pump intervals, and p50/p95/p99
input-to-authoritative-update latency. Vary player count, burst size, loss, jitter,
and browser. Separate time waiting on the network from time waiting for the next
pump/game tick. Keep upstream crypto/SCTP changes isolated and reproduce their
benefits with the same workload. The Discordium fork's buffer patch is not proof
of a str0m defect without a failing case against the published dependency.
