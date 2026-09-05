# Optional Bevy network debug panel

Enable `bevy-debug-ui` on `renet-cross` to import the Bevy **0.18** plugin. It uses only Bevy's built-in `Node`, `Text`, and `Button` components. Transport-only users can enable `packet-conditioner` without depending on Bevy. The existing game example remains on Bevy 0.19 and uses built-in UI for its FPS overlay, network metrics, and graphs.

```rust,ignore
use renet_cross::conditioner::ConditionerHandle;
use renet_cross::bevy_debug::{ConditionerDebug, ConditionerDebugPlugin};

let handle = ConditionerHandle::default();
transport.set_conditioner(handle.clone());
app.add_plugins(ConditionerDebugPlugin::new(handle));
```

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
interception, draining, and session cleanup. `packet_io/mod.rs` selects either the
conditioned implementation or a zero-sized, allocation-free passthrough at compile
time. Scheduling, clocks, shared state, and queue lifecycle live behind that
interface. Each transport has one feature-gated extension impl for its public
conditioner controls; receive/send loops contain no conditioner feature guards.

The enabled adapter shares its state with browser callbacks and preserves native
`Send + Sync`. Dropping its last owner clears externally visible queue statistics.
The feature is optional; these APIs are available starting with 0.5.0.
