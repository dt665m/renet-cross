# Renet 2.0.0 fork provenance

Source: crates.io `renet` 2.0.0, checksum
`8535f2f4b5afe5e31fc37be4b8a1f167e61177f356b1c6ee8456b7045d6c74c0`.
Upstream: https://github.com/lucaspoffo/renet (release commit `bc4a387`).
The source and upstream tests are preserved except for the crate name in tests;
the README describes this fork. License files and rustfmt
configuration come from the upstream checkout. The echo example, which requires
an omitted sibling transport dependency, is not included.

renet-cross depends directly on this published core and re-exports it as `renet`.
This is a library correction, not a second acknowledgment mechanism in the game.

## ACK output follows receipt events

Retaining receipt history is different from needing to transmit a receipt now.
Data packets (including duplicate/retransmitted data) mark an ACK as pending.
The next `get_packets_to_send` emits it immediately. An ACK-only packet updates
existing bookkeeping but does not request another ACK. Retained receipts are also
included as a separate companion ACK when there is outgoing data, allowing the
existing ACK-of-ACK history retirement to work without continuous idle traffic.
No sleep, batching deadline, or new wire representation is introduced.

ACK-only packets remain in the sent-packet map for history retirement and expire
through the existing timeout. They are excluded from the data-loss denominator
and RTT samples because their confirmation may wait for later data. Byte-rate
statistics continue to include all serialized traffic. Data loss remains an
estimate based on receipt of transport acknowledgments, not an exact measure of
network-layer drops or gameplay acceptance.
RTT remains zero until the first data packet is acknowledged and retains its
last sample during receive-only periods; ACK-only traffic cannot refresh it.

The wire representation is unchanged. Reliable retry/ordering/fragmentation
remain Renet-owned. Both endpoints should run the patch to eliminate the old
endpoint's repeated ACK output; this is not a new application protocol.

## Verification

`cargo test -p renet-cross-core` covers immediate ACKs without advancing time, idle settling
under 1 kHz servicing, recovery after a lost ACK, duplicate/out-of-order data,
one-way receipt-history retirement, and data loss/RTT/byte accounting, in addition
to the original channel/packet tests. Workspace tests exercise the shared patch
through actual native transport and simulation paths.

## Statistics window generation

Clear all crossed 300 ms buckets when time advances, including a complete six-second
wrap to the same ring index. Validate ACKs against the original bucket generation,
so a late ACK cannot credit a reused bucket. Regression tests cover suspension,
partial wrap, and ACK arrival across bucket expiry.

The loss estimate describes outgoing data without an observed ACK, excluding the
current and previous two buckets (a 600–900 ms grace period). Missing or delayed
return acknowledgments can affect it as well as outgoing data loss; it is not a
receiver-side or IP-layer loss counter. These window fixes do not establish the
cause of the residual production estimate.
