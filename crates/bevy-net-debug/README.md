# bevy-net-debug

Optional Bevy 0.18 debug panel for `renet-cross` client packet conditioning. This separate crate uses Bevy's built-in UI and requires Rust 1.89 or newer. The transport crate has no Bevy dependency.

```toml
[dependencies]
renet-cross = "0.6"
bevy-net-debug = "0.1"
```

Use Bevy 0.18 in the consuming application.

Attach one shared control handle to the client transport and the plugin:

```rust,ignore
use renet_cross::{ClientTransportConfig, conditioner::ConditionerHandle};
use bevy_net_debug::{ConditionerDebug, ConditionerDebugPlugin};
use std::time::Duration;

let handle = ConditionerHandle::default();
let config = ClientTransportConfig { conditioner: Some(handle.clone()) };
let transport = renet_cross::UdpNetcodeClientTransport::new_with_config(
    current_time, authentication, socket, config,
)?;
app.add_plugins(ConditionerDebugPlugin::new(handle));
```

Your app supplies Bevy's render/UI plugins and a UI camera. The debug plugin does not install `DefaultPlugins` or create a camera. After each transport update, report Renet's measured transport RTT to the `ConditionerDebug` resource:

```rust,ignore
if client.is_connected() {
    debug.report_rtt(Some(Duration::from_secs_f64(client.rtt())));
} else {
    debug.report_rtt(None);
}
```

Pass `None` on disconnect and before replacing a transport so calibration does not carry across sessions. Set `debug.visible = false` to hide the panel without changing impairment.

The panel supports Off, approximate 150/300 ms total RTT targets, custom added delay, jitter, packet loss, brief outage, and recalibration. It displays baseline and observed RTT, settings, and separate incoming/outgoing queue and drop metrics. RTT targets require an unconditioned baseline and add `max(0, (target - baseline) / 2)` delay in each direction. Baseline sampling freezes while impairment is active and waits five seconds after disabling it to limit contamination from smoothed RTT samples. This is an approximation, not a guarantee that Renet's filter has completely settled. Transport RTT is distinct from gameplay input acknowledgement age.

Conditioning happens in `renet-cross`, below gameplay messages; the panel only controls its public API. Browser conditioning operates at the WebRTC DataChannel boundary and does not impair ICE/DTLS/SCTP establishment. Use the transport crate's headless API for server conditioning or applications without Bevy.

Licensed under the repository's `LICENSE`; this package does not change those terms.
