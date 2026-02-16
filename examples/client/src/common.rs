use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use bevy::dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin, FrameTimeGraphConfig};
use bevy::log::{Level, LogPlugin};
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use bevy_feathers::{
    FeathersPlugins,
    dark_theme::create_dark_theme,
    theme::{ThemeBackgroundColor, ThemedText, UiTheme},
    tokens,
};
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;
use renet::{DefaultChannel, RenetClient};
use shared::{
    decode, encode, ClientInput, EntityKind, EntityState, JoinSnapshot, WorldDelta, BASE_PLAYER_SPEED, WORLD_HEIGHT,
    WORLD_WIDTH,
};

#[cfg(not(target_arch = "wasm32"))]
use crate::client_native::NativeUdpTransport as PlatformTransport;
#[cfg(target_arch = "wasm32")]
use crate::client_web::WebRtcNetcodeTransport as PlatformTransport;

pub const PROTOCOL_ID: u64 = 7;
const WORLD_BORDER_THICKNESS: f32 = 14.0;
const CLIENT_PREDICTION_HZ: f64 = 120.0;
const MAX_LOCAL_PREDICTION_STEP: f32 = 1.0 / 30.0;
const MAX_REMOTE_EXTRAPOLATION_STEP: f32 = 1.0 / 20.0;
const STOP_SNAP_DISTANCE: f32 = 4.0;
const STOP_SNAP_CORRECTION_MAX: f32 = 0.22;
const NET_GRAPH_SAMPLES: usize = 72;
const NET_GRAPH_HEIGHT_PX: f32 = 26.0;
const NET_GRAPH_BAR_WIDTH_PX: f32 = 1.0;
const NET_GRAPH_BAR_GAP_PX: f32 = 1.0;
const INPUT_SEND_INTERVAL_SECONDS: f32 = 1.0 / 30.0;
const APP_TRAFFIC_SAMPLE_SECONDS: f32 = 0.10;
const MAX_NETWORK_DT_SECONDS: f32 = 0.10;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Resource, Parser)]
#[command(name = "client")]
struct NativeClientArgs {
    #[arg(long, env = "NET_HTTP_BASE", default_value = "http://127.0.0.1:8080")]
    http_base: String,
}

#[derive(Resource, Default)]
struct RenderIndex {
    by_id: HashMap<u64, Entity>,
    ghost_you: Option<Entity>,
}

#[derive(Resource, Default)]
struct WorldView {
    tick: u32,
    you: Option<u64>,
    authoritative: HashMap<u64, EntityState>,
    rendered: HashMap<u64, EntityState>,
}

#[derive(Resource, Default, Clone, Copy)]
struct LocalInputState {
    dir: [f32; 2],
}

#[derive(Resource, Default)]
struct AppTrafficMeter {
    tx_bytes_accum: u64,
    rx_bytes_accum: u64,
    elapsed: f32,
    tx_rate_kib_s: f32,
    rx_rate_kib_s: f32,
}

#[derive(Resource)]
struct NetGraphHistory {
    rtt_ms: VecDeque<f32>,
    loss_pct: VecDeque<f32>,
    up_kib_s: VecDeque<f32>,
    down_kib_s: VecDeque<f32>,
}

impl Default for NetGraphHistory {
    fn default() -> Self {
        Self {
            rtt_ms: VecDeque::with_capacity(NET_GRAPH_SAMPLES),
            loss_pct: VecDeque::with_capacity(NET_GRAPH_SAMPLES),
            up_kib_s: VecDeque::with_capacity(NET_GRAPH_SAMPLES),
            down_kib_s: VecDeque::with_capacity(NET_GRAPH_SAMPLES),
        }
    }
}

impl NetGraphHistory {
    fn push(&mut self, rtt_ms: f32, loss_pct: f32, up_kib_s: f32, down_kib_s: f32) {
        push_capped(&mut self.rtt_ms, rtt_ms);
        push_capped(&mut self.loss_pct, loss_pct);
        push_capped(&mut self.up_kib_s, up_kib_s);
        push_capped(&mut self.down_kib_s, down_kib_s);
    }
}

impl WorldView {
    fn apply_snapshot(&mut self, snapshot: JoinSnapshot) {
        self.tick = snapshot.tick;
        self.you = Some(snapshot.you);

        let entities = snapshot.world.entities;
        let count = entities.len();

        self.authoritative.clear();
        self.rendered.clear();
        self.authoritative.reserve(count);
        self.rendered.reserve(count);

        for entity in entities {
            self.rendered.insert(entity.id, entity.clone());
            self.authoritative.insert(entity.id, entity);
        }
    }

    fn apply_delta(&mut self, delta: WorldDelta) {
        self.tick = delta.tick;

        let authoritative_free = self.authoritative.capacity().saturating_sub(self.authoritative.len());
        let authoritative_needed = delta.upserts.len().saturating_sub(authoritative_free);
        if authoritative_needed > 0 {
            self.authoritative.reserve(authoritative_needed);
        }

        let rendered_free = self.rendered.capacity().saturating_sub(self.rendered.len());
        let rendered_needed = delta.upserts.len().saturating_sub(rendered_free);
        if rendered_needed > 0 {
            self.rendered.reserve(rendered_needed);
        }

        for entity in delta.upserts {
            self.authoritative.insert(entity.id, entity.clone());
            self.rendered.entry(entity.id).or_insert(entity);
        }
        for id in delta.removed {
            self.authoritative.remove(&id);
            self.rendered.remove(&id);
        }
    }

    fn fixed_predict_local(&mut self, dt_secs: f32, local_input: [f32; 2]) {
        if dt_secs <= 0.0 {
            return;
        }

        let Some(you) = self.you else {
            return;
        };
        let Some(authoritative) = self.authoritative.get(&you).cloned() else {
            return;
        };
        if authoritative.kind != EntityKind::Player {
            return;
        }

        let rendered = self.rendered.entry(you).or_insert_with(|| authoritative.clone());
        rendered.kind = authoritative.kind;
        rendered.mass = authoritative.mass;
        rendered.radius = authoritative.radius;

        let local_dir = normalize(local_input);
        let local_moving = local_dir[0].abs() > f32::EPSILON || local_dir[1].abs() > f32::EPSILON;
        if !local_moving {
            rendered.vel = [0.0, 0.0];
            return;
        }

        let speed = BASE_PLAYER_SPEED / (1.0 + authoritative.mass * 0.015);
        rendered.vel = [local_dir[0] * speed, local_dir[1] * speed];
        let step_dt = dt_secs.min(MAX_LOCAL_PREDICTION_STEP);
        rendered.pos[0] = clamp_axis(rendered.pos[0] + rendered.vel[0] * step_dt, WORLD_WIDTH);
        rendered.pos[1] = clamp_axis(rendered.pos[1] + rendered.vel[1] * step_dt, WORLD_HEIGHT);
    }

    fn step_smoothing(&mut self, dt_secs: f32, local_input: [f32; 2]) {
        if dt_secs <= 0.0 {
            return;
        }

        let remote_correction = smooth_step_factor(dt_secs, 10.0);
        let local_correction = smooth_step_factor(dt_secs, 6.0);
        let pellet_correction = smooth_step_factor(dt_secs, 18.0);
        let local_dir = normalize(local_input);
        let local_moving = local_dir[0].abs() > f32::EPSILON || local_dir[1].abs() > f32::EPSILON;
        let remote_extrapolation_dt = dt_secs.min(MAX_REMOTE_EXTRAPOLATION_STEP);

        for (id, authoritative) in &self.authoritative {
            let rendered = self
                .rendered
                .entry(*id)
                .or_insert_with(|| authoritative.clone());

            rendered.kind = authoritative.kind;
            rendered.mass = authoritative.mass;
            rendered.radius = authoritative.radius;

            match authoritative.kind {
                EntityKind::Player if self.you == Some(*id) => {
                    if local_moving {
                        let speed = BASE_PLAYER_SPEED / (1.0 + authoritative.mass * 0.015);
                        rendered.vel = [local_dir[0] * speed, local_dir[1] * speed];
                        let error_x = authoritative.pos[0] - rendered.pos[0];
                        let error_y = authoritative.pos[1] - rendered.pos[1];
                        let forward_error = error_x * local_dir[0] + error_y * local_dir[1];
                        let forward_x = local_dir[0] * forward_error;
                        let forward_y = local_dir[1] * forward_error;
                        let lateral_x = error_x - forward_x;
                        let lateral_y = error_y - forward_y;

                        // Keep local controls tight by preferring one-way forward correction,
                        // but always clean up lateral drift so circling/turning doesn't diverge.
                        let lateral_correction = (local_correction * 2.0).min(0.45);
                        rendered.pos[0] += lateral_x * lateral_correction;
                        rendered.pos[1] += lateral_y * lateral_correction;

                        if forward_error > 0.0 {
                            rendered.pos[0] += forward_x * local_correction;
                            rendered.pos[1] += forward_y * local_correction;
                        } else {
                            let ahead_correction = (local_correction * 0.12).min(0.04);
                            rendered.pos[0] += forward_x * ahead_correction;
                            rendered.pos[1] += forward_y * ahead_correction;
                        }
                    } else {
                        rendered.vel = [0.0, 0.0];
                        let dx = authoritative.pos[0] - rendered.pos[0];
                        let dy = authoritative.pos[1] - rendered.pos[1];
                        if dx * dx + dy * dy <= STOP_SNAP_DISTANCE * STOP_SNAP_DISTANCE {
                            rendered.pos[0] = authoritative.pos[0];
                            rendered.pos[1] = authoritative.pos[1];
                        } else {
                            let stop_correction = smooth_step_factor(dt_secs, 8.0).min(STOP_SNAP_CORRECTION_MAX);
                            rendered.pos[0] += dx * stop_correction;
                            rendered.pos[1] += dy * stop_correction;
                        }
                    }
                }
                EntityKind::Player => {
                    rendered.pos[0] += authoritative.vel[0] * remote_extrapolation_dt;
                    rendered.pos[1] += authoritative.vel[1] * remote_extrapolation_dt;
                    rendered.pos[0] += (authoritative.pos[0] - rendered.pos[0]) * remote_correction;
                    rendered.pos[1] += (authoritative.pos[1] - rendered.pos[1]) * remote_correction;
                    rendered.vel = authoritative.vel;
                }
                EntityKind::Pellet => {
                    rendered.pos[0] += (authoritative.pos[0] - rendered.pos[0]) * pellet_correction;
                    rendered.pos[1] += (authoritative.pos[1] - rendered.pos[1]) * pellet_correction;
                    rendered.vel = authoritative.vel;
                }
            }

            rendered.pos[0] = clamp_axis(rendered.pos[0], WORLD_WIDTH);
            rendered.pos[1] = clamp_axis(rendered.pos[1], WORLD_HEIGHT);
        }

        self.rendered
            .retain(|id, _| self.authoritative.contains_key(id));
    }
}

struct ClientRuntime {
    client_id: u64,
    renet: RenetClient,
    transport: PlatformTransport,
    input_seq: u32,
    last_sent_input: [f32; 2],
    input_send_elapsed: f32,
}

impl ClientRuntime {
    fn new(client_id: u64, renet: RenetClient, transport: PlatformTransport) -> Self {
        Self {
            client_id,
            renet,
            transport,
            input_seq: 0,
            last_sent_input: [0.0, 0.0],
            input_send_elapsed: INPUT_SEND_INTERVAL_SECONDS,
        }
    }
}

#[derive(Component)]
struct VisualEntity {
    target: Vec2,
    radius: f32,
}

#[derive(Component)]
struct GhostVisual;

#[derive(Component)]
struct MainCamera;

#[derive(Component)]
struct NetStatsPanelText;

#[derive(Component)]
struct NetDiagPanelRoot;

#[derive(Resource)]
struct NetDiagState {
    enabled: bool,
}

impl Default for NetDiagState {
    fn default() -> Self {
        Self { enabled: false }
    }
}

#[derive(Resource, Default)]
struct NetDiagUiState {
    root: Option<Entity>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NetMetricKind {
    Rtt,
    Loss,
    Up,
    Down,
}

#[derive(Component)]
struct NetMetricValueText {
    kind: NetMetricKind,
}

#[derive(Component)]
struct NetMetricBar {
    kind: NetMetricKind,
    sample_idx: usize,
}

#[cfg(target_arch = "wasm32")]
struct PendingWebBootstrap {
    slot: std::rc::Rc<std::cell::RefCell<Option<Result<(RenetClient, PlatformTransport, u64), String>>>>,
}

#[cfg(target_arch = "wasm32")]
fn default_log_filter() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| {
        "info,client=info,renet=warn,renetcode=warn,renet_server=warn,shared=info,wgpu=warn,naga=warn"
            .to_string()
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn default_log_filter() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| {
        "info,client=info,renet=warn,renetcode=warn,renet_server=warn,shared=info,wgpu=warn,naga=warn"
            .to_string()
    })
}

pub fn run() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
    }

    let mut app = App::new();
    app.add_plugins((
        DefaultPlugins.set(LogPlugin {
            // Bevy log plugin uses tracing-wasm on wasm targets, so this also drives browser console logs.
            level: Level::INFO,
            filter: default_log_filter(),
            ..Default::default()
        }),
        FeathersPlugins,
        FpsOverlayPlugin {
            config: FpsOverlayConfig {
                text_config: TextFont {
                    font_size: 16.0,
                    ..Default::default()
                },
                text_color: Color::srgb(0.86, 1.0, 0.9),
                refresh_interval: core::time::Duration::from_millis(75),
                enabled: true,
                frame_time_graph_config: FrameTimeGraphConfig {
                    enabled: false,
                    min_fps: 30.0,
                    target_fps: 60.0,
                },
            },
        },
    ))
    .insert_resource(UiTheme(create_dark_theme()))
    .insert_resource(bevy::time::Time::<bevy::time::Fixed>::from_hz(CLIENT_PREDICTION_HZ))
    .insert_resource(ClearColor(Color::srgb(0.03, 0.03, 0.05)))
    .insert_resource(WorldView::default())
    .insert_resource(LocalInputState::default())
    .insert_resource(NetDiagState::default())
    .insert_resource(NetDiagUiState::default())
    .insert_resource(AppTrafficMeter::default())
    .insert_resource(NetGraphHistory::default())
    .insert_resource(RenderIndex::default())
    .add_systems(Startup, setup_scene)
    .add_systems(Startup, startup_connect)
    .add_systems(FixedUpdate, fixed_prediction_tick)
    .add_systems(Update, poll_connect)
    .add_systems(Update, network_tick)
    .add_systems(Update, toggle_network_diag)
    .add_systems(
        Update,
        update_network_panel
            .run_if(net_diag_enabled)
            .run_if(on_timer(Duration::from_millis(100))),
    )
    .add_systems(Update, sync_world_to_scene)
    .add_systems(Update, follow_local_player_camera)
    .add_systems(Update, animate_visuals);

    #[cfg(not(target_arch = "wasm32"))]
    app.insert_resource(NativeClientArgs::parse());

    app.run();
}

fn setup_scene(mut commands: Commands, diag_state: Res<NetDiagState>, mut diag_ui: ResMut<NetDiagUiState>) {
    commands.spawn((Camera2d, MainCamera));
    spawn_world_borders(&mut commands);
    if diag_state.enabled {
        diag_ui.root = Some(spawn_network_panel(&mut commands));
    }
}

fn spawn_world_borders(commands: &mut Commands) {
    let half_w = WORLD_WIDTH * 0.5;
    let half_h = WORLD_HEIGHT * 0.5;
    let t = WORLD_BORDER_THICKNESS;
    let color = Color::srgba(0.96, 0.96, 1.0, 0.42);

    commands.spawn((
        Sprite {
            color,
            custom_size: Some(Vec2::new(WORLD_WIDTH + t * 2.0, t)),
            ..Default::default()
        },
        Transform::from_xyz(0.0, half_h, 0.4),
    ));

    commands.spawn((
        Sprite {
            color,
            custom_size: Some(Vec2::new(WORLD_WIDTH + t * 2.0, t)),
            ..Default::default()
        },
        Transform::from_xyz(0.0, -half_h, 0.4),
    ));

    commands.spawn((
        Sprite {
            color,
            custom_size: Some(Vec2::new(t, WORLD_HEIGHT + t * 2.0)),
            ..Default::default()
        },
        Transform::from_xyz(-half_w, 0.0, 0.4),
    ));

    commands.spawn((
        Sprite {
            color,
            custom_size: Some(Vec2::new(t, WORLD_HEIGHT + t * 2.0)),
            ..Default::default()
        },
        Transform::from_xyz(half_w, 0.0, 0.4),
    ));
}

fn spawn_network_panel(commands: &mut Commands) -> Entity {
    let graph_width_px = NET_GRAPH_SAMPLES as f32 * NET_GRAPH_BAR_WIDTH_PX
        + NET_GRAPH_SAMPLES.saturating_sub(1) as f32 * NET_GRAPH_BAR_GAP_PX;

    commands
        .spawn((
            Node {
            position_type: PositionType::Absolute,
            left: Val::Px(14.0),
            top: Val::Px(76.0),
            width: Val::Px(graph_width_px + 92.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(6.0),
            padding: UiRect::all(Val::Px(6.0)),
                ..Default::default()
            },
            ThemeBackgroundColor(tokens::WINDOW_BG),
        ))
        .insert(NetDiagPanelRoot)
        .with_children(|parent| {
            parent.spawn((
                Text::new("Net Diagnostics"),
                ThemedText,
                TextColor(Color::srgb(0.95, 0.97, 1.0)),
            ));
            parent.spawn((
                Text::new("bootstrapping..."),
                ThemedText,
                TextColor(Color::srgb(0.72, 0.78, 0.9)),
                NetStatsPanelText,
            ));

            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(4.0),
                        ..Default::default()
                    },
                ))
                .with_children(|metric| {
                    metric.spawn((
                        Text::new("rtt: --"),
                        ThemedText,
                        TextColor(Color::srgb(0.72, 0.78, 0.9)),
                        NetMetricValueText { kind: NetMetricKind::Rtt },
                    ));
                    metric
                        .spawn((
                            Node {
                                width: Val::Px(graph_width_px),
                                height: Val::Px(NET_GRAPH_HEIGHT_PX),
                                display: Display::Flex,
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::End,
                                column_gap: Val::Px(NET_GRAPH_BAR_GAP_PX),
                                ..Default::default()
                            },
                            BackgroundColor(Color::srgba(0.06, 0.08, 0.14, 0.92)),
                        ))
                        .with_children(|bars| {
                            for i in 0..NET_GRAPH_SAMPLES {
                                bars.spawn((
                                    Node {
                                        width: Val::Px(NET_GRAPH_BAR_WIDTH_PX),
                                        height: Val::Px(1.0),
                                        ..Default::default()
                                    },
                                    BackgroundColor(Color::srgba(0.49, 0.78, 0.99, 0.96)),
                                    NetMetricBar {
                                        kind: NetMetricKind::Rtt,
                                        sample_idx: i,
                                    },
                                ));
                            }
                        });
                });

            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(4.0),
                        ..Default::default()
                    },
                ))
                .with_children(|metric| {
                    metric.spawn((
                        Text::new("loss: --"),
                        ThemedText,
                        TextColor(Color::srgb(0.72, 0.78, 0.9)),
                        NetMetricValueText {
                            kind: NetMetricKind::Loss,
                        },
                    ));
                    metric
                        .spawn((
                            Node {
                                width: Val::Px(graph_width_px),
                                height: Val::Px(NET_GRAPH_HEIGHT_PX),
                                display: Display::Flex,
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::End,
                                column_gap: Val::Px(NET_GRAPH_BAR_GAP_PX),
                                ..Default::default()
                            },
                            BackgroundColor(Color::srgba(0.06, 0.08, 0.14, 0.92)),
                        ))
                        .with_children(|bars| {
                            for i in 0..NET_GRAPH_SAMPLES {
                                bars.spawn((
                                    Node {
                                        width: Val::Px(NET_GRAPH_BAR_WIDTH_PX),
                                        height: Val::Px(1.0),
                                        ..Default::default()
                                    },
                                    BackgroundColor(Color::srgba(0.98, 0.49, 0.5, 0.96)),
                                    NetMetricBar {
                                        kind: NetMetricKind::Loss,
                                        sample_idx: i,
                                    },
                                ));
                            }
                        });
                });

            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(4.0),
                        ..Default::default()
                    },
                ))
                .with_children(|metric| {
                    metric.spawn((
                        Text::new("up: --"),
                        ThemedText,
                        TextColor(Color::srgb(0.72, 0.78, 0.9)),
                        NetMetricValueText { kind: NetMetricKind::Up },
                    ));
                    metric
                        .spawn((
                            Node {
                                width: Val::Px(graph_width_px),
                                height: Val::Px(NET_GRAPH_HEIGHT_PX),
                                display: Display::Flex,
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::End,
                                column_gap: Val::Px(NET_GRAPH_BAR_GAP_PX),
                                ..Default::default()
                            },
                            BackgroundColor(Color::srgba(0.06, 0.08, 0.14, 0.92)),
                        ))
                        .with_children(|bars| {
                            for i in 0..NET_GRAPH_SAMPLES {
                                bars.spawn((
                                    Node {
                                        width: Val::Px(NET_GRAPH_BAR_WIDTH_PX),
                                        height: Val::Px(1.0),
                                        ..Default::default()
                                    },
                                    BackgroundColor(Color::srgba(0.54, 0.92, 0.62, 0.96)),
                                    NetMetricBar {
                                        kind: NetMetricKind::Up,
                                        sample_idx: i,
                                    },
                                ));
                            }
                        });
                });

            parent
                .spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(4.0),
                        ..Default::default()
                    },
                ))
                .with_children(|metric| {
                    metric.spawn((
                        Text::new("down: --"),
                        ThemedText,
                        TextColor(Color::srgb(0.72, 0.78, 0.9)),
                        NetMetricValueText {
                            kind: NetMetricKind::Down,
                        },
                    ));
                    metric
                        .spawn((
                            Node {
                                width: Val::Px(graph_width_px),
                                height: Val::Px(NET_GRAPH_HEIGHT_PX),
                                display: Display::Flex,
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::End,
                                column_gap: Val::Px(NET_GRAPH_BAR_GAP_PX),
                                ..Default::default()
                            },
                            BackgroundColor(Color::srgba(0.06, 0.08, 0.14, 0.92)),
                        ))
                        .with_children(|bars| {
                            for i in 0..NET_GRAPH_SAMPLES {
                                bars.spawn((
                                    Node {
                                        width: Val::Px(NET_GRAPH_BAR_WIDTH_PX),
                                        height: Val::Px(1.0),
                                        ..Default::default()
                                    },
                                    BackgroundColor(Color::srgba(0.98, 0.85, 0.46, 0.96)),
                                    NetMetricBar {
                                        kind: NetMetricKind::Down,
                                        sample_idx: i,
                                    },
                                ));
                            }
                        });
                });
        })
        .id()
}

#[cfg(not(target_arch = "wasm32"))]
fn startup_connect(world: &mut World) {
    let http_base = world.resource::<NativeClientArgs>().http_base.clone();
    log::info!("native bootstrap start: base_http={http_base}");
    match crate::client_native::connect(&http_base, PROTOCOL_ID) {
        Ok((renet, transport, client_id)) => {
            log::info!("native client connected to bootstrap endpoint {http_base} with client_id={client_id}");
            world.insert_non_send_resource(ClientRuntime::new(client_id, renet, transport));
        }
        Err(err) => {
            log::error!("native bootstrap failed: {err}");
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn startup_connect(world: &mut World) {
    let http_base = default_http_base();
    log::info!("web bootstrap start: base_http={http_base}");
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot_clone = std::rc::Rc::clone(&slot);

    wasm_bindgen_futures::spawn_local(async move {
        let result = crate::client_web::connect(&http_base, PROTOCOL_ID).await;
        *slot_clone.borrow_mut() = Some(result);
    });

    world.insert_non_send_resource(PendingWebBootstrap { slot });
}

#[cfg(not(target_arch = "wasm32"))]
fn poll_connect() {}

#[cfg(target_arch = "wasm32")]
fn poll_connect(world: &mut World) {
    let result = {
        let Some(pending) = world.get_non_send_resource_mut::<PendingWebBootstrap>() else {
            return;
        };
        pending.slot.borrow_mut().take()
    };

    let Some(result) = result else {
        return;
    };

    match result {
        Ok((renet, transport, client_id)) => {
            log::info!("web client connected with client_id={client_id}");
            world.insert_non_send_resource(ClientRuntime::new(client_id, renet, transport));
        }
        Err(err) => {
            log::error!("web bootstrap failed: {err}");
        }
    }
    world.remove_non_send_resource::<PendingWebBootstrap>();
}

fn net_diag_enabled(state: Option<Res<NetDiagState>>) -> bool {
    state.map(|state| state.enabled).unwrap_or(true)
}

fn toggle_network_diag(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<NetDiagState>,
    mut ui_state: ResMut<NetDiagUiState>,
    mut commands: Commands,
) {
    if !keyboard.just_pressed(KeyCode::KeyQ) {
        return;
    }

    state.enabled = !state.enabled;
    if state.enabled {
        if ui_state.root.is_none() {
            ui_state.root = Some(spawn_network_panel(&mut commands));
        }
    } else if let Some(root) = ui_state.root.take() {
        commands.entity(root).despawn();
    }

    log::info!(
        "network diagnostics {} (toggle with Q)",
        if state.enabled { "enabled" } else { "disabled" }
    );
}

fn update_network_panel(
    runtime: Option<NonSend<ClientRuntime>>,
    world: Res<WorldView>,
    traffic: Res<AppTrafficMeter>,
    graphs: Res<NetGraphHistory>,
    mut panel_text: Query<&mut Text, (With<NetStatsPanelText>, Without<NetMetricValueText>)>,
    mut metric_values: Query<(&NetMetricValueText, &mut Text), (With<NetMetricValueText>, Without<NetStatsPanelText>)>,
    mut metric_bars: Query<(&NetMetricBar, &mut Node)>,
) {
    let Ok(mut panel_text) = panel_text.single_mut() else {
        return;
    };

    let transport = if cfg!(target_arch = "wasm32") {
        "webrtc"
    } else {
        "udp"
    };

    if let Some(runtime) = runtime {
        let status = if runtime.renet.is_connected() {
            "connected"
        } else if runtime.renet.is_connecting() {
            "connecting"
        } else {
            "disconnected"
        };
        let info = runtime.renet.network_info();
        let rtt_ms = (info.rtt * 1000.0) as f32;
        let loss_pct = (info.packet_loss * 100.0) as f32;
        let up_kib_s = traffic.tx_rate_kib_s;
        let down_kib_s = traffic.rx_rate_kib_s;
        let renet_up_kib_s = (info.bytes_sent_per_second / 1024.0) as f32;
        let renet_down_kib_s = (info.bytes_received_per_second / 1024.0) as f32;

        let panel_summary = format!(
            "transport: {transport}\nstatus: {status}\nclient_id: {}\nworld_tick: {}\nrtt/loss: {:.1}ms / {:.2}%\napp up/down: {:.1} / {:.1} KiB/s\nrenet avg: {:.1} / {:.1} KiB/s",
            runtime.client_id,
            world.tick,
            rtt_ms,
            loss_pct,
            up_kib_s,
            down_kib_s,
            renet_up_kib_s,
            renet_down_kib_s,
        );
        if panel_text.0 != panel_summary {
            panel_text.0 = panel_summary;
        }
    } else {
        let panel_summary = format!(
            "transport: {transport}\nstatus: bootstrapping\nclient_id: -\nworld_tick: {}\nrtt: -\nloss: -\nup: -\ndown: -",
            world.tick
        );
        if panel_text.0 != panel_summary {
            panel_text.0 = panel_summary;
        }
    }

    let rtt_latest = graphs.rtt_ms.back().copied().unwrap_or(0.0);
    let loss_latest = graphs.loss_pct.back().copied().unwrap_or(0.0);
    let up_latest = graphs.up_kib_s.back().copied().unwrap_or(0.0);
    let down_latest = graphs.down_kib_s.back().copied().unwrap_or(0.0);

    let rtt_top = graph_top(&graphs.rtt_ms, 12.0);
    let loss_top = graph_top(&graphs.loss_pct, 1.0);
    let up_top = graph_top(&graphs.up_kib_s, 1.0);
    let down_top = graph_top(&graphs.down_kib_s, 1.0);

    for (metric_text, mut text) in &mut metric_values {
        let value_text = match metric_text.kind {
            NetMetricKind::Rtt => format!("rtt: {:>7.1} ms   scale {:>7.1}", rtt_latest, rtt_top),
            NetMetricKind::Loss => format!("loss:{:>7.2}%    scale {:>7.2}", loss_latest, loss_top),
            NetMetricKind::Up => format!("up:  {:>7.1} KiB/s scale {:>7.1}", up_latest, up_top),
            NetMetricKind::Down => format!("down:{:>7.1} KiB/s scale {:>7.1}", down_latest, down_top),
        };
        if text.0 != value_text {
            text.0 = value_text;
        }
    }

    if graphs.is_changed() {
        for (metric_bar, mut node) in &mut metric_bars {
            let (samples, top) = match metric_bar.kind {
                NetMetricKind::Rtt => (&graphs.rtt_ms, rtt_top),
                NetMetricKind::Loss => (&graphs.loss_pct, loss_top),
                NetMetricKind::Up => (&graphs.up_kib_s, up_top),
                NetMetricKind::Down => (&graphs.down_kib_s, down_top),
            };
            let value = sample_at_column(samples, metric_bar.sample_idx);
            let normalized = if top <= f32::EPSILON {
                0.0
            } else {
                (value / top).clamp(0.0, 1.0)
            };
            let next_height = Val::Px((normalized * NET_GRAPH_HEIGHT_PX).max(1.0));
            if node.height != next_height {
                node.height = next_height;
            }
        }
    }
}

fn push_capped(buffer: &mut VecDeque<f32>, value: f32) {
    if buffer.len() == NET_GRAPH_SAMPLES {
        buffer.pop_front();
    }
    buffer.push_back(value.max(0.0));
}

fn graph_top(buffer: &VecDeque<f32>, floor: f32) -> f32 {
    let mut top = floor;
    for value in buffer {
        if *value > top {
            top = *value;
        }
    }
    top.max(floor) * 1.15
}

fn sample_at_column(samples: &VecDeque<f32>, sample_idx: usize) -> f32 {
    let len = samples.len();
    if len == 0 {
        return 0.0;
    }
    if len >= NET_GRAPH_SAMPLES {
        return samples[sample_idx.min(len - 1)];
    }
    let left_pad = NET_GRAPH_SAMPLES - len;
    if sample_idx < left_pad {
        0.0
    } else {
        samples[sample_idx - left_pad]
    }
}

fn network_tick(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    touches: Res<Touches>,
    primary_window: Query<&Window, With<bevy::window::PrimaryWindow>>,
    runtime: Option<NonSendMut<ClientRuntime>>,
    diag_state: Res<NetDiagState>,
    mut traffic: ResMut<AppTrafficMeter>,
    mut graphs: ResMut<NetGraphHistory>,
    mut input_state: ResMut<LocalInputState>,
    mut world: ResMut<WorldView>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };

    let runtime = &mut *runtime;
    let ClientRuntime {
        client_id,
        renet,
        transport,
        input_seq,
        last_sent_input,
        input_send_elapsed,
    } = runtime;

    let raw_dt = time.delta();
    let dt = Duration::from_secs_f32(raw_dt.as_secs_f32().min(MAX_NETWORK_DT_SECONDS));
    let diag_enabled = diag_state.enabled;
    renet.update(dt);

    if let Err(err) = transport.update(dt, renet) {
        log::warn!("transport update error (client_id={}): {err}", *client_id);
    }

    let movement_input = read_movement_input(&keyboard, &touches, primary_window.iter().next());
    input_state.dir = movement_input;

    if renet.is_connected() {
        let moving = movement_input[0].abs() > f32::EPSILON || movement_input[1].abs() > f32::EPSILON;
        *input_send_elapsed += dt.as_secs_f32();
        if *input_send_elapsed >= INPUT_SEND_INTERVAL_SECONDS {
            *input_seq = input_seq.wrapping_add(1);
            let input = ClientInput {
                seq: *input_seq,
                move_dir: movement_input,
            };
            if moving && input.seq % 30 == 0 {
                log::debug!(
                    "client input client_id={} seq={} dir=[{:.2}, {:.2}]",
                    *client_id,
                    input.seq,
                    input.move_dir[0],
                    input.move_dir[1]
                );
            }
            let payload = encode(&input);
            if diag_enabled {
                traffic.tx_bytes_accum = traffic.tx_bytes_accum.saturating_add(payload.len() as u64);
            }
            renet.send_message(DefaultChannel::Unreliable, payload);
            *last_sent_input = movement_input;
            *input_send_elapsed = (*input_send_elapsed - INPUT_SEND_INTERVAL_SECONDS).min(INPUT_SEND_INTERVAL_SECONDS);
        }
    }

    while let Some(bytes) = renet.receive_message(DefaultChannel::ReliableOrdered) {
        if diag_enabled {
            traffic.rx_bytes_accum = traffic.rx_bytes_accum.saturating_add(bytes.len() as u64);
        }
        match decode::<JoinSnapshot>(&bytes) {
            Ok(snapshot) => {
                *client_id = snapshot.you;
                log::info!(
                    "applied JoinSnapshot: you={} tick={} entities={}",
                    snapshot.you,
                    snapshot.tick,
                    snapshot.world.entities.len()
                );
                world.apply_snapshot(snapshot);
            }
            Err(err) => {
                log::warn!("failed to decode JoinSnapshot: {err} ({} bytes)", bytes.len());
            }
        }
    }

    while let Some(bytes) = renet.receive_message(DefaultChannel::Unreliable) {
        if diag_enabled {
            traffic.rx_bytes_accum = traffic.rx_bytes_accum.saturating_add(bytes.len() as u64);
        }
        match decode::<WorldDelta>(&bytes) {
            Ok(delta) => {
                log::trace!(
                    "applied WorldDelta: tick={} upserts={} removed={} events={}",
                    delta.tick,
                    delta.upserts.len(),
                    delta.removed.len(),
                    delta.events.len()
                );
                world.apply_delta(delta);
            }
            Err(err) => {
                log::warn!("failed to decode WorldDelta: {err} ({} bytes)", bytes.len());
            }
        }
    }

    world.step_smoothing(dt.as_secs_f32(), movement_input);

    if diag_enabled {
        traffic.elapsed += dt.as_secs_f32();
        if traffic.elapsed >= APP_TRAFFIC_SAMPLE_SECONDS {
            let sample_secs = traffic.elapsed.max(0.001);
            traffic.tx_rate_kib_s = traffic.tx_bytes_accum as f32 / 1024.0 / sample_secs;
            traffic.rx_rate_kib_s = traffic.rx_bytes_accum as f32 / 1024.0 / sample_secs;
            traffic.tx_bytes_accum = 0;
            traffic.rx_bytes_accum = 0;
            traffic.elapsed = 0.0;
            let network_info = renet.network_info();
            let rtt_ms = (network_info.rtt * 1000.0) as f32;
            let loss_pct = (network_info.packet_loss * 100.0) as f32;
            graphs.push(rtt_ms, loss_pct, traffic.tx_rate_kib_s, traffic.rx_rate_kib_s);
        }
    } else {
        traffic.tx_bytes_accum = 0;
        traffic.rx_bytes_accum = 0;
        traffic.elapsed = 0.0;
        traffic.tx_rate_kib_s = 0.0;
        traffic.rx_rate_kib_s = 0.0;
    }

    if renet.is_connected() {
        if let Err(err) = transport.send_packets(renet) {
            log::warn!("transport send error (client_id={}): {err}", *client_id);
        }
    }
}

fn fixed_prediction_tick(
    fixed_time: Res<bevy::time::Time<bevy::time::Fixed>>,
    input_state: Res<LocalInputState>,
    mut world: ResMut<WorldView>,
) {
    world.fixed_predict_local(fixed_time.delta_secs(), input_state.dir);
}

fn follow_local_player_camera(world: Res<WorldView>, mut cameras: Query<&mut Transform, With<MainCamera>>) {
    let Some(you) = world.you else {
        return;
    };
    let Some(player) = world.rendered.get(&you) else {
        return;
    };

    let target = world_to_canvas(player.pos);
    for mut transform in &mut cameras {
        transform.translation.x = target.x;
        transform.translation.y = target.y;
    }
}

fn sync_world_to_scene(
    mut commands: Commands,
    time: Res<Time>,
    world: Res<WorldView>,
    mut render_index: ResMut<RenderIndex>,
    mut visuals: Query<(&mut VisualEntity, &mut Sprite, &mut Transform, Option<&GhostVisual>)>,
) {
    if !world.is_changed() {
        return;
    }

    let current_render_count = render_index.by_id.len();
    let target_render_count = world.rendered.len();
    if current_render_count < target_render_count {
        render_index
            .by_id
            .reserve(target_render_count - current_render_count);
    }

    for entity_state in world.rendered.values() {
        if let Some(entity) = render_index.by_id.get(&entity_state.id).copied() {
            if let Ok((mut visual, mut sprite, mut transform, _ghost)) = visuals.get_mut(entity) {
                let target = world_to_canvas(entity_state.pos);
                visual.target = target;
                visual.radius = entity_state.radius;
                let next_size = Vec2::splat(entity_state.radius * 2.0);
                if sprite.custom_size != Some(next_size) {
                    sprite.custom_size = Some(next_size);
                }
                let next_color = entity_color(entity_state, world.you == Some(entity_state.id));
                if sprite.color != next_color {
                    sprite.color = next_color;
                }
                if world.you == Some(entity_state.id) {
                    transform.translation.x = target.x;
                    transform.translation.y = target.y;
                }
            }
            continue;
        }

        let target = world_to_canvas(entity_state.pos);
        let color = entity_color(entity_state, world.you == Some(entity_state.id));
        let spawned = commands
            .spawn((
                Sprite {
                    color,
                    custom_size: Some(Vec2::splat(entity_state.radius * 2.0)),
                    ..Default::default()
                },
                Transform::from_xyz(target.x, target.y, z_for_kind(entity_state.kind)),
                VisualEntity {
                    target,
                    radius: entity_state.radius,
                },
            ))
            .id();

        render_index.by_id.insert(entity_state.id, spawned);
    }

    let stale_ids: Vec<u64> = render_index
        .by_id
        .keys()
        .copied()
        .filter(|id| !world.rendered.contains_key(id))
        .collect();

    for id in stale_ids {
        if let Some(entity) = render_index.by_id.remove(&id) {
            commands.entity(entity).despawn();
        }
    }

    let authoritative_you = world.you.and_then(|you| world.authoritative.get(&you));
    match authoritative_you {
        Some(entity_state) => {
            let target = world_to_canvas(entity_state.pos);
            if let Some(ghost_entity) = render_index.ghost_you {
                if let Ok((mut visual, mut sprite, mut transform, _ghost)) = visuals.get_mut(ghost_entity) {
                    let dt_secs = time.delta_secs().max(0.0);
                    let current = Vec2::new(transform.translation.x, transform.translation.y);
                    let error = target - current;
                    let velocity = Vec2::new(entity_state.vel[0], entity_state.vel[1]);

                    let next = if velocity.length_squared() > f32::EPSILON {
                        let dir = velocity.normalize();
                        let forward_error = dir * error.dot(dir);
                        let lateral_error = error - forward_error;
                        let forward_alpha = (dt_secs * 10.0).clamp(0.0, 1.0);
                        let lateral_alpha = (dt_secs * 30.0).clamp(0.0, 1.0);
                        current + forward_error * forward_alpha + lateral_error * lateral_alpha
                    } else {
                        current.lerp(target, (dt_secs * 12.0).clamp(0.0, 1.0))
                    };

                    transform.translation.x = next.x;
                    transform.translation.y = next.y;
                    visual.target = next;
                    visual.radius = entity_state.radius;
                    let next_size = Vec2::splat(entity_state.radius * 2.0);
                    if sprite.custom_size != Some(next_size) {
                        sprite.custom_size = Some(next_size);
                    }
                    let next_color = ghost_color(entity_state);
                    if sprite.color != next_color {
                        sprite.color = next_color;
                    }
                }
            } else {
                let spawned = commands
                    .spawn((
                        Sprite {
                            color: ghost_color(entity_state),
                            custom_size: Some(Vec2::splat(entity_state.radius * 2.0)),
                            ..Default::default()
                        },
                        Transform::from_xyz(target.x, target.y, z_for_kind(EntityKind::Player) - 0.02),
                        VisualEntity {
                            target,
                            radius: entity_state.radius,
                        },
                        GhostVisual,
                    ))
                    .id();
                render_index.ghost_you = Some(spawned);
            }
        }
        None => {
            if let Some(entity) = render_index.ghost_you.take() {
                commands.entity(entity).despawn();
            }
        }
    }
}

fn animate_visuals(time: Res<Time>, mut query: Query<(&VisualEntity, &mut Transform), Without<GhostVisual>>) {
    let alpha = (time.delta_secs() * 12.0).clamp(0.0, 1.0);

    for (visual, mut transform) in &mut query {
        let target = Vec3::new(visual.target.x, visual.target.y, transform.translation.z);
        transform.translation = transform.translation.lerp(target, alpha);
    }
}

fn read_movement_input(
    keyboard: &ButtonInput<KeyCode>,
    touches: &Touches,
    primary_window: Option<&Window>,
) -> [f32; 2] {
    let mut dir = [0.0_f32, 0.0_f32];

    if let Some(window) = primary_window {
        if let Some(touch) = touches.iter().next() {
            let center = Vec2::new(window.width() * 0.5, window.height() * 0.5);
            let touch_pos = touch.position();
            let raw = Vec2::new(touch_pos.x - center.x, center.y - touch_pos.y);

            if raw.length_squared() > 64.0 {
                let normalized = raw.normalize();
                return [normalized.x, normalized.y];
            }
            return [0.0, 0.0];
        }
    }

    if keyboard.pressed(KeyCode::KeyW) || keyboard.pressed(KeyCode::ArrowUp) {
        dir[1] += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) || keyboard.pressed(KeyCode::ArrowDown) {
        dir[1] -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        dir[0] -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        dir[0] += 1.0;
    }

    dir
}

fn world_to_canvas(pos: [f32; 2]) -> Vec2 {
    Vec2::new(pos[0] - WORLD_WIDTH * 0.5, pos[1] - WORLD_HEIGHT * 0.5)
}

fn entity_color(entity: &EntityState, is_you: bool) -> Color {
    let base = Color::srgb(entity.color[0], entity.color[1], entity.color[2]);
    match entity.kind {
        EntityKind::Player if is_you => {
            // Keep local player visually distinguishable without overriding server-authoritative color.
            base.mix(&Color::srgb(1.0, 1.0, 1.0), 0.10)
        }
        EntityKind::Player | EntityKind::Pellet => base,
    }
}

fn ghost_color(entity: &EntityState) -> Color {
    Color::srgba(entity.color[0], entity.color[1], entity.color[2], 0.18)
}

fn z_for_kind(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Player => 2.0,
        EntityKind::Pellet => 1.0,
    }
}

fn smooth_step_factor(dt_secs: f32, hz: f32) -> f32 {
    (1.0 - (-hz * dt_secs).exp()).clamp(0.0, 1.0)
}

fn normalize(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= f32::EPSILON {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

fn clamp_axis(value: f32, max: f32) -> f32 {
    value.clamp(0.0, max)
}

#[cfg(target_arch = "wasm32")]
fn default_http_base() -> String {
    if let Some(configured) = option_env!("NET_WEB_HTTP_BASE") {
        let trimmed = configured.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    web_sys::window()
        .and_then(|window| window.location().origin().ok())
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string())
}
