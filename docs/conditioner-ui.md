# Optional Bevy network debug panel

The Bevy **0.19.1** plugin lives in the separate `bevy-net-debug` workspace
crate. The `renet-cross` transport crate has no Bevy dependency, even with all
its features enabled. Packet tooling is always available through runtime config;
no `packet-conditioner` or `bevy-debug-ui` Cargo feature is needed or provided.
Use `renet-cross = "0.6"` and `bevy-net-debug = "0.2"`.
The companion requires Rust 1.95; core retains its Rust 1.88 declaration.

```rust,ignore
use renet_cross::{ClientTransportConfig, conditioner::ConditionerHandle};
use bevy_net_debug::{ConditionerDebug, ConditionerDebugPlugin};

let handle = ConditionerHandle::default();
let config = ClientTransportConfig { conditioner: Some(handle.clone()) };
let transport = UdpNetcodeClientTransport::new_with_config(
    current_time, authentication, socket, config,
)?;
app.add_plugins(ConditionerDebugPlugin::new(handle));
```

For HTTP/bootstrap helpers set `NativeConnectOptions.transport` or
`WebRtcConnectOptions.transport` to that same config. The handle is installed
before the first netcode handshake. None is the default passthrough; a disabled
handle permits later runtime configuration. Its stats describe conditioner
queues/drops, not a packet capture or a measurement of real network loss.

Use the same handle attached to your native or browser transport. Your application supplies its usual Bevy UI/render plugins and camera. This plugin does not install `DefaultPlugins` or spawn a camera. Its panel appears at the top right; set the `ConditionerDebug` resource's `visible` field to false to hide it without changing conditioning.

After each Renet transport update, report the current transport RTT through the resource:

```rust,ignore
let sample = client.is_connected().then(|| {
    std::time::Duration::from_secs_f64(client.rtt())
});
conditioner_debug.report_rtt(sample);
```

This explicit integration also works when your network runtime is a non-send resource on WASM. Keep reporting while the panel is hidden. Pass `None` while disconnected and before replacing a transport to discard session telemetry; attach the handle to the replacement transport separately. Input acknowledgement age is a gameplay metric and must not be passed as Renet RTT.

The panel offers Off, approximate 150/300 ms total RTT, custom per-direction delay in 10 ms increments, symmetric jitter in 5 ms increments, loss in 1% increments, a one-second outage, and baseline recalibration. Custom values can also be set precisely through the handle's `configure` API. Queue limits and seeds remain explicit in that API rather than occupying the small panel.

Targets use `max(0, (target - baseline) / 2)` added delay in each direction. A target below baseline adds no delay and displays an explanation. Presets retain your current jitter/loss settings; the panel shows them alongside delay and measured RTT. They cannot guarantee an exact observed RTT.

Baseline samples are accepted only with impairment inactive. The estimate freezes while conditioning is active. Turning impairment off starts a five-second settling interval before accepting new baseline samples, limiting contamination from Renet's smoothed RTT. Recalibrate turns conditioning off and clears the estimate. Five seconds is a heuristic, not proof that Renet's historical samples have expired; let traffic stabilize before treating the estimate as representative. Until a baseline is available, target buttons leave the transport configuration unchanged. Zero RTT samples are excluded.

Statistics distinguish simulated loss, outage drops, queue overflow, and packets discarded during transitions, with incoming/outgoing packet and byte queue depths. These are conditioner counters, not a claim to measure real network loss.

The browser boundary delays complete DataChannel messages carrying Renet/netcode packets. It exercises their acknowledgement, retransmission, and timeout behavior, but does not impair underlying ICE/DTLS/SCTP establishment. Polling cadence, jitter, browser buffering, and real network conditions still influence observed RTT. Wire-level impairment testing remains complementary.

## Internal structure

Transport pumps call the private `packet_io::PacketGate` interface for raw packet
interception, draining, and session cleanup. The adapter uses runtime controls;
there is no conditioner feature selection or duplicate disabled implementation.
It shares state with browser callbacks and preserves native `Send + Sync`.
Dropping its last owner clears externally visible queue statistics. The UI crate
uses only public transport controls and receives measured Renet RTT from the app.

The panel uses individual ECS metric cards and aligned Incoming/Outgoing queue and drop cells. It wraps within 95% of viewport width, is capped at 94% of viewport height, and scrolls with the mouse wheel over the panel when needed. Labels use ASCII for default-font compatibility; hiding the panel does not change network settings.
