use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use bevy::dev_tools::fps_overlay::{FpsOverlayConfig, FpsOverlayPlugin, FrameTimeGraphConfig};
use bevy::ecs::hierarchy::ChildSpawnerCommands;
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
    BASE_PLAYER_SPEED, ClientInput, EntityKind, EntityState, JoinSnapshot, WORLD_HEIGHT,
    WORLD_WIDTH, WorldDelta, decode, encode, is_newer_input_seq,
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
const INPUT_SEND_INTERVAL_SECONDS: f32 = 1.0 / 60.0;
const INPUT_DIRECTION_CHANGE_DOT_THRESHOLD: f32 = 0.995;
const MAX_PENDING_INPUT_COMMANDS: usize = 256;
const MAX_INPUT_SEND_CATCH_UP_STEPS: usize = 4;
const APP_TRAFFIC_SAMPLE_SECONDS: f32 = 0.10;
const MAX_NETWORK_DT_SECONDS: f32 = 0.10;
const MAX_SMOOTHING_DT_SECONDS: f32 = 1.0 / 30.0;

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

#[derive(Clone, Copy)]
struct PendingInputCommand {
    seq: u32,
    move_dir: [f32; 2],
    sent_at_seconds: f32,
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

#[derive(Clone, Copy)]
struct SmoothingParams {
    remote_correction: f32,
    local_correction: f32,
    pellet_correction: f32,
    stop_correction: f32,
    local_dir: [f32; 2],
    local_moving: bool,
    remote_extrapolation_dt: f32,
}

impl SmoothingParams {
    fn new(dt_secs: f32, local_input: [f32; 2]) -> Self {
        let local_dir = normalize(local_input);
        let local_moving = has_movement_input(local_dir);
        Self {
            remote_correction: bounded_smoothing_factor(dt_secs, 10.0, 0.45),
            local_correction: bounded_smoothing_factor(dt_secs, 6.0, 0.45),
            pellet_correction: bounded_smoothing_factor(dt_secs, 18.0, 0.65),
            stop_correction: bounded_smoothing_factor(dt_secs, 8.0, STOP_SNAP_CORRECTION_MAX),
            local_dir,
            local_moving,
            remote_extrapolation_dt: dt_secs.min(MAX_REMOTE_EXTRAPOLATION_STEP),
        }
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

        let authoritative_free = self
            .authoritative
            .capacity()
            .saturating_sub(self.authoritative.len());
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

        let rendered = self
            .rendered
            .entry(you)
            .or_insert_with(|| authoritative.clone());
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

        let params = SmoothingParams::new(dt_secs.min(MAX_SMOOTHING_DT_SECONDS), local_input);

        for (id, authoritative) in &self.authoritative {
            let rendered = self
                .rendered
                .entry(*id)
                .or_insert_with(|| authoritative.clone());
            Self::update_rendered_from_authoritative_base(rendered, authoritative);

            match authoritative.kind {
                EntityKind::Player if self.you == Some(*id) => {
                    Self::smooth_local_player(rendered, authoritative, params)
                }
                EntityKind::Player => Self::smooth_remote_player(rendered, authoritative, params),
                EntityKind::Pellet => Self::smooth_pellet(rendered, authoritative, params),
            }

            Self::clamp_rendered_entity(rendered);
        }

        self.rendered
            .retain(|id, _| self.authoritative.contains_key(id));
    }

    fn reconcile_local_prediction(
        &mut self,
        pending_inputs: &VecDeque<PendingInputCommand>,
        now_seconds: f32,
    ) {
        let Some((you, authoritative)) = self.local_authoritative_player() else {
            return;
        };
        self.reset_local_rendered_to_authoritative(you, &authoritative);
        self.replay_pending_inputs(you, &authoritative, pending_inputs, now_seconds);
    }

    fn local_authoritative_player(&self) -> Option<(u64, EntityState)> {
        let you = self.you?;
        let authoritative = self.authoritative.get(&you)?.clone();
        if authoritative.kind != EntityKind::Player {
            return None;
        }
        Some((you, authoritative))
    }

    fn update_rendered_from_authoritative_base(
        rendered: &mut EntityState,
        authoritative: &EntityState,
    ) {
        rendered.kind = authoritative.kind;
        rendered.mass = authoritative.mass;
        rendered.radius = authoritative.radius;
    }

    fn smooth_local_player(
        rendered: &mut EntityState,
        authoritative: &EntityState,
        params: SmoothingParams,
    ) {
        if params.local_moving {
            let speed = BASE_PLAYER_SPEED / (1.0 + authoritative.mass * 0.015);
            rendered.vel = [params.local_dir[0] * speed, params.local_dir[1] * speed];

            let error_x = authoritative.pos[0] - rendered.pos[0];
            let error_y = authoritative.pos[1] - rendered.pos[1];
            let forward_error = error_x * params.local_dir[0] + error_y * params.local_dir[1];
            let forward_x = params.local_dir[0] * forward_error;
            let forward_y = params.local_dir[1] * forward_error;
            let lateral_x = error_x - forward_x;
            let lateral_y = error_y - forward_y;

            let lateral_correction = (params.local_correction * 2.0).min(0.45);
            rendered.pos[0] += lateral_x * lateral_correction;
            rendered.pos[1] += lateral_y * lateral_correction;

            if forward_error > 0.0 {
                rendered.pos[0] += forward_x * params.local_correction;
                rendered.pos[1] += forward_y * params.local_correction;
            } else {
                let ahead_correction = (params.local_correction * 0.12).min(0.04);
                rendered.pos[0] += forward_x * ahead_correction;
                rendered.pos[1] += forward_y * ahead_correction;
            }
            return;
        }

        rendered.vel = [0.0, 0.0];
        let dx = authoritative.pos[0] - rendered.pos[0];
        let dy = authoritative.pos[1] - rendered.pos[1];
        if dx * dx + dy * dy <= STOP_SNAP_DISTANCE * STOP_SNAP_DISTANCE {
            rendered.pos = authoritative.pos;
            return;
        }

        rendered.pos[0] += dx * params.stop_correction;
        rendered.pos[1] += dy * params.stop_correction;
    }

    fn smooth_remote_player(
        rendered: &mut EntityState,
        authoritative: &EntityState,
        params: SmoothingParams,
    ) {
        rendered.pos[0] += authoritative.vel[0] * params.remote_extrapolation_dt;
        rendered.pos[1] += authoritative.vel[1] * params.remote_extrapolation_dt;
        rendered.pos[0] += (authoritative.pos[0] - rendered.pos[0]) * params.remote_correction;
        rendered.pos[1] += (authoritative.pos[1] - rendered.pos[1]) * params.remote_correction;
        rendered.vel = authoritative.vel;
    }

    fn smooth_pellet(
        rendered: &mut EntityState,
        authoritative: &EntityState,
        params: SmoothingParams,
    ) {
        rendered.pos[0] += (authoritative.pos[0] - rendered.pos[0]) * params.pellet_correction;
        rendered.pos[1] += (authoritative.pos[1] - rendered.pos[1]) * params.pellet_correction;
        rendered.vel = authoritative.vel;
    }

    fn clamp_rendered_entity(rendered: &mut EntityState) {
        rendered.pos[0] = clamp_axis(rendered.pos[0], WORLD_WIDTH);
        rendered.pos[1] = clamp_axis(rendered.pos[1], WORLD_HEIGHT);
    }

    fn reset_local_rendered_to_authoritative(&mut self, you: u64, authoritative: &EntityState) {
        let rendered = self
            .rendered
            .entry(you)
            .or_insert_with(|| authoritative.clone());
        *rendered = authoritative.clone();
    }

    fn replay_pending_inputs(
        &mut self,
        you: u64,
        authoritative: &EntityState,
        pending_inputs: &VecDeque<PendingInputCommand>,
        now_seconds: f32,
    ) {
        if pending_inputs.is_empty() {
            return;
        }

        let speed = BASE_PLAYER_SPEED / (1.0 + authoritative.mass * 0.015);
        let Some(rendered) = self.rendered.get_mut(&you) else {
            return;
        };

        for (index, command) in pending_inputs.iter().enumerate() {
            let start = command.sent_at_seconds.min(now_seconds);
            let end = pending_inputs
                .get(index + 1)
                .map(|next| next.sent_at_seconds)
                .unwrap_or(now_seconds)
                .min(now_seconds);
            let mut remaining = (end - start).max(0.0);
            if remaining <= 0.0 {
                continue;
            }

            let dir = normalize(command.move_dir);
            let vel = if has_movement_input(dir) {
                [dir[0] * speed, dir[1] * speed]
            } else {
                [0.0, 0.0]
            };
            rendered.vel = vel;

            while remaining > 0.0 {
                let step_dt = remaining.min(MAX_LOCAL_PREDICTION_STEP);
                rendered.pos[0] = clamp_axis(rendered.pos[0] + vel[0] * step_dt, WORLD_WIDTH);
                rendered.pos[1] = clamp_axis(rendered.pos[1] + vel[1] * step_dt, WORLD_HEIGHT);
                remaining -= step_dt;
            }
        }
    }
}

struct ClientRuntime {
    client_id: u64,
    renet: RenetClient,
    transport: PlatformTransport,
    input_seq: u32,
    last_sent_input: [f32; 2],
    input_send_elapsed: f32,
    clock_seconds: f32,
    pending_inputs: VecDeque<PendingInputCommand>,
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
            clock_seconds: 0.0,
            pending_inputs: VecDeque::with_capacity(MAX_PENDING_INPUT_COMMANDS),
        }
    }
}

struct NetworkTickCtx<'a> {
    runtime: &'a mut ClientRuntime,
    world: &'a mut WorldView,
    traffic: &'a mut AppTrafficMeter,
    graphs: &'a mut NetGraphHistory,
    dt: Duration,
    movement_input: [f32; 2],
    diag_enabled: bool,
    received_authoritative: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct InputSendCadence {
    periodic_sends: usize,
    residual_elapsed: f32,
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

#[derive(Resource, Default)]
struct NetDiagState {
    enabled: bool,
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

#[derive(Clone, Copy)]
struct NetMetricSpec {
    kind: NetMetricKind,
    label: &'static str,
    value_color: [f32; 3],
    bar_color: [f32; 4],
    floor: f32,
}

const NET_METRICS: [NetMetricSpec; 4] = [
    NetMetricSpec {
        kind: NetMetricKind::Rtt,
        label: "rtt",
        value_color: [0.72, 0.78, 0.9],
        bar_color: [0.49, 0.78, 0.99, 0.96],
        floor: 12.0,
    },
    NetMetricSpec {
        kind: NetMetricKind::Loss,
        label: "loss",
        value_color: [0.72, 0.78, 0.9],
        bar_color: [0.98, 0.49, 0.5, 0.96],
        floor: 1.0,
    },
    NetMetricSpec {
        kind: NetMetricKind::Up,
        label: "up",
        value_color: [0.72, 0.78, 0.9],
        bar_color: [0.54, 0.92, 0.62, 0.96],
        floor: 1.0,
    },
    NetMetricSpec {
        kind: NetMetricKind::Down,
        label: "down",
        value_color: [0.72, 0.78, 0.9],
        bar_color: [0.98, 0.85, 0.46, 0.96],
        floor: 1.0,
    },
];

#[derive(Component)]
struct NetMetricValueText {
    kind: NetMetricKind,
}

#[derive(Component)]
struct NetMetricBar {
    kind: NetMetricKind,
    sample_idx: usize,
}

type NetMetricValueQuery<'w, 's> = Query<
    'w,
    's,
    (&'static NetMetricValueText, &'static mut Text),
    (With<NetMetricValueText>, Without<NetStatsPanelText>),
>;

type SceneVisualQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut VisualEntity,
        &'static mut Sprite,
        &'static mut Transform,
        Option<&'static GhostVisual>,
    ),
>;

#[cfg(target_arch = "wasm32")]
struct PendingWebBootstrap {
    slot: std::rc::Rc<
        std::cell::RefCell<Option<Result<(RenetClient, PlatformTransport, u64), String>>>,
    >,
}

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
    .insert_resource(bevy::time::Time::<bevy::time::Fixed>::from_hz(
        CLIENT_PREDICTION_HZ,
    ))
    .insert_resource(ClearColor(Color::srgb(0.03, 0.03, 0.05)))
    .insert_resource(WorldView::default())
    .insert_resource(LocalInputState::default())
    .insert_resource(NetDiagState::default())
    .insert_resource(NetDiagUiState::default())
    .insert_resource(AppTrafficMeter::default())
    .insert_resource(NetGraphHistory::default())
    .insert_resource(RenderIndex::default())
    .add_systems(Startup, (setup_scene, startup_connect))
    .add_systems(PreUpdate, (sample_local_input,))
    .add_systems(FixedUpdate, (fixed_prediction_tick,))
    .add_systems(
        Update,
        (
            // Bootstrap and transport runtime.
            poll_connect,
            network_tick,
            // Network diagnostics UI.
            toggle_network_diag,
            update_network_panel
                .run_if(net_diag_enabled)
                .run_if(on_timer(Duration::from_millis(100))),
            // Scene sync and presentation.
            sync_world_to_scene,
            follow_local_player_camera,
            animate_visuals,
        ),
    );

    #[cfg(not(target_arch = "wasm32"))]
    app.insert_resource(NativeClientArgs::parse());

    app.run();
}

fn setup_scene(
    mut commands: Commands,
    diag_state: Res<NetDiagState>,
    mut diag_ui: ResMut<NetDiagUiState>,
) {
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
    let graph_width_px = metric_graph_width_px();

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
            for spec in NET_METRICS {
                spawn_metric_section(parent, graph_width_px, spec);
            }
        })
        .id()
}

fn metric_graph_width_px() -> f32 {
    NET_GRAPH_SAMPLES as f32 * NET_GRAPH_BAR_WIDTH_PX
        + NET_GRAPH_SAMPLES.saturating_sub(1) as f32 * NET_GRAPH_BAR_GAP_PX
}

fn spawn_metric_section(
    parent: &mut ChildSpawnerCommands<'_>,
    graph_width_px: f32,
    spec: NetMetricSpec,
) {
    parent
        .spawn((Node {
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(4.0),
            ..Default::default()
        },))
        .with_children(|metric| {
            metric.spawn((
                Text::new(format!("{}: --", spec.label)),
                ThemedText,
                TextColor(Color::srgb(
                    spec.value_color[0],
                    spec.value_color[1],
                    spec.value_color[2],
                )),
                NetMetricValueText { kind: spec.kind },
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
                    for sample_idx in 0..NET_GRAPH_SAMPLES {
                        bars.spawn((
                            Node {
                                width: Val::Px(NET_GRAPH_BAR_WIDTH_PX),
                                height: Val::Px(1.0),
                                ..Default::default()
                            },
                            BackgroundColor(Color::srgba(
                                spec.bar_color[0],
                                spec.bar_color[1],
                                spec.bar_color[2],
                                spec.bar_color[3],
                            )),
                            NetMetricBar {
                                kind: spec.kind,
                                sample_idx,
                            },
                        ));
                    }
                });
        });
}

#[cfg(not(target_arch = "wasm32"))]
fn startup_connect(world: &mut World) {
    let http_base = world.resource::<NativeClientArgs>().http_base.clone();
    log::info!("native bootstrap start: base_http={http_base}");
    match crate::client_native::connect(&http_base, PROTOCOL_ID) {
        Ok((renet, transport, client_id)) => {
            on_bootstrap_success(world, "native", client_id, renet, transport)
        }
        Err(err) => on_bootstrap_error("native", &err),
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
            on_bootstrap_success(world, "web", client_id, renet, transport);
        }
        Err(err) => on_bootstrap_error("web", &err),
    }
    world.remove_non_send_resource::<PendingWebBootstrap>();
}

fn on_bootstrap_success(
    world: &mut World,
    mode: &str,
    client_id: u64,
    renet: RenetClient,
    transport: PlatformTransport,
) {
    log::info!("{mode} client connected with client_id={client_id}");
    world.insert_non_send_resource(ClientRuntime::new(client_id, renet, transport));
}

fn on_bootstrap_error(mode: &str, err: &str) {
    log::error!("{mode} bootstrap failed: {err}");
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
    mut metric_values: NetMetricValueQuery<'_, '_>,
    mut metric_bars: Query<(&NetMetricBar, &mut Node)>,
) {
    let Ok(mut panel_text) = panel_text.single_mut() else {
        return;
    };

    let summary = build_panel_summary(runtime.as_deref(), &world, &traffic);
    if panel_text.0 != summary {
        panel_text.0 = summary;
    }
    update_metric_value_texts(&graphs, &mut metric_values);
    if graphs.is_changed() {
        update_metric_bar_heights(&graphs, &mut metric_bars);
    }
}

fn build_panel_summary(
    runtime: Option<&ClientRuntime>,
    world: &WorldView,
    traffic: &AppTrafficMeter,
) -> String {
    let transport = if cfg!(target_arch = "wasm32") {
        "webrtc"
    } else {
        "udp"
    };

    let Some(runtime) = runtime else {
        return format!(
            "transport: {transport}\nstatus: bootstrapping\nclient_id: -\nworld_tick: {}\nrtt: -\nloss: -\nup: -\ndown: -",
            world.tick
        );
    };

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
    let renet_up_kib_s = (info.bytes_sent_per_second / 1024.0) as f32;
    let renet_down_kib_s = (info.bytes_received_per_second / 1024.0) as f32;

    format!(
        "transport: {transport}\nstatus: {status}\nclient_id: {}\nworld_tick: {}\nrtt/loss: {:.1}ms / {:.2}%\napp up/down: {:.1} / {:.1} KiB/s\nrenet avg: {:.1} / {:.1} KiB/s",
        runtime.client_id,
        world.tick,
        rtt_ms,
        loss_pct,
        traffic.tx_rate_kib_s,
        traffic.rx_rate_kib_s,
        renet_up_kib_s,
        renet_down_kib_s,
    )
}

fn update_metric_value_texts(
    graphs: &NetGraphHistory,
    metric_values: &mut NetMetricValueQuery<'_, '_>,
) {
    for (metric_text, mut text) in metric_values.iter_mut() {
        let spec = metric_spec(metric_text.kind);
        let (series, scale_top) = metric_series_and_scale(graphs, spec);
        let latest = series.back().copied().unwrap_or(0.0);
        let value_text = format_metric_value(spec, latest, scale_top);
        if text.0 != value_text {
            text.0 = value_text;
        }
    }
}

fn update_metric_bar_heights(
    graphs: &NetGraphHistory,
    metric_bars: &mut Query<(&NetMetricBar, &mut Node)>,
) {
    for (metric_bar, mut node) in metric_bars.iter_mut() {
        let spec = metric_spec(metric_bar.kind);
        let (series, scale_top) = metric_series_and_scale(graphs, spec);
        let value = sample_at_column(series, metric_bar.sample_idx);
        let normalized = if scale_top <= f32::EPSILON {
            0.0
        } else {
            (value / scale_top).clamp(0.0, 1.0)
        };
        let next_height = Val::Px((normalized * NET_GRAPH_HEIGHT_PX).max(1.0));
        if node.height != next_height {
            node.height = next_height;
        }
    }
}

fn metric_spec(kind: NetMetricKind) -> NetMetricSpec {
    match kind {
        NetMetricKind::Rtt => NET_METRICS[0],
        NetMetricKind::Loss => NET_METRICS[1],
        NetMetricKind::Up => NET_METRICS[2],
        NetMetricKind::Down => NET_METRICS[3],
    }
}

fn format_metric_value(spec: NetMetricSpec, latest: f32, scale_top: f32) -> String {
    match spec.kind {
        NetMetricKind::Rtt => format!("rtt: {:>7.1} ms   scale {:>7.1}", latest, scale_top),
        NetMetricKind::Loss => format!("loss:{:>7.2}%    scale {:>7.2}", latest, scale_top),
        NetMetricKind::Up => format!("up:  {:>7.1} KiB/s scale {:>7.1}", latest, scale_top),
        NetMetricKind::Down => format!("down:{:>7.1} KiB/s scale {:>7.1}", latest, scale_top),
    }
}

fn metric_series_and_scale<'a>(
    graphs: &'a NetGraphHistory,
    spec: NetMetricSpec,
) -> (&'a VecDeque<f32>, f32) {
    let series = match spec.kind {
        NetMetricKind::Rtt => &graphs.rtt_ms,
        NetMetricKind::Loss => &graphs.loss_pct,
        NetMetricKind::Up => &graphs.up_kib_s,
        NetMetricKind::Down => &graphs.down_kib_s,
    };
    let scale_top = graph_top(series, spec.floor);
    (series, scale_top)
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
    input_state: Res<LocalInputState>,
    runtime: Option<NonSendMut<ClientRuntime>>,
    diag_state: Res<NetDiagState>,
    mut traffic: ResMut<AppTrafficMeter>,
    mut graphs: ResMut<NetGraphHistory>,
    mut world: ResMut<WorldView>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };

    let dt = Duration::from_secs_f32(time.delta().as_secs_f32().min(MAX_NETWORK_DT_SECONDS));
    let mut ctx = NetworkTickCtx {
        runtime: &mut runtime,
        world: &mut world,
        traffic: &mut traffic,
        graphs: &mut graphs,
        dt,
        movement_input: input_state.dir,
        diag_enabled: diag_state.enabled,
        received_authoritative: false,
    };

    prepare_frame(&mut ctx);
    pump_transport_update(&mut ctx);
    send_local_input(&mut ctx);
    receive_authoritative_messages(&mut ctx);
    reconcile_and_smooth_world(&mut ctx);
    update_network_diagnostics(&mut ctx);
    flush_transport_send(&mut ctx);
}

fn prepare_frame(ctx: &mut NetworkTickCtx<'_>) {
    ctx.runtime.clock_seconds += ctx.dt.as_secs_f32();
    ctx.runtime.renet.update(ctx.dt);
}

fn pump_transport_update(ctx: &mut NetworkTickCtx<'_>) {
    if let Err(err) = ctx.runtime.transport.update(ctx.dt, &mut ctx.runtime.renet) {
        log::warn!(
            "transport update error (client_id={}): {err}",
            ctx.runtime.client_id
        );
    }
}

fn send_local_input(ctx: &mut NetworkTickCtx<'_>) {
    if !ctx.runtime.renet.is_connected() {
        return;
    }

    ctx.runtime.input_send_elapsed += ctx.dt.as_secs_f32();
    let edge_sent = if should_edge_send_input(ctx.runtime.last_sent_input, ctx.movement_input) {
        send_input_command(ctx, ctx.movement_input);
        true
    } else {
        false
    };

    let cadence = plan_periodic_input_sends(
        ctx.runtime.input_send_elapsed,
        INPUT_SEND_INTERVAL_SECONDS,
        MAX_INPUT_SEND_CATCH_UP_STEPS,
        edge_sent,
    );
    for _ in 0..cadence.periodic_sends {
        send_input_command(ctx, ctx.movement_input);
    }
    ctx.runtime.input_send_elapsed = cadence.residual_elapsed;
}

fn send_input_command(ctx: &mut NetworkTickCtx<'_>, move_dir: [f32; 2]) {
    ctx.runtime.input_seq = ctx.runtime.input_seq.wrapping_add(1);
    let input = ClientInput {
        seq: ctx.runtime.input_seq,
        move_dir,
    };
    if has_movement_input(input.move_dir) && input.seq.is_multiple_of(30) {
        log::debug!(
            "client input client_id={} seq={} dir=[{:.2}, {:.2}]",
            ctx.runtime.client_id,
            input.seq,
            input.move_dir[0],
            input.move_dir[1]
        );
    }

    let payload = encode(&input);
    if ctx.diag_enabled {
        ctx.traffic.tx_bytes_accum = ctx
            .traffic
            .tx_bytes_accum
            .saturating_add(payload.len() as u64);
    }
    ctx.runtime
        .renet
        .send_message(DefaultChannel::Unreliable, payload);

    ctx.runtime.pending_inputs.push_back(PendingInputCommand {
        seq: input.seq,
        move_dir: input.move_dir,
        sent_at_seconds: ctx.runtime.clock_seconds,
    });
    while ctx.runtime.pending_inputs.len() > MAX_PENDING_INPUT_COMMANDS {
        ctx.runtime.pending_inputs.pop_front();
    }
    ctx.runtime.last_sent_input = move_dir;
}

fn plan_periodic_input_sends(
    elapsed: f32,
    interval: f32,
    max_steps: usize,
    edge_sent: bool,
) -> InputSendCadence {
    if interval <= f32::EPSILON {
        return InputSendCadence {
            periodic_sends: 0,
            residual_elapsed: 0.0,
        };
    }

    let mut residual_elapsed = elapsed.max(0.0);
    if edge_sent && residual_elapsed >= interval {
        residual_elapsed -= interval;
    }

    let mut periodic_sends = 0;
    while residual_elapsed >= interval && periodic_sends < max_steps {
        periodic_sends += 1;
        residual_elapsed -= interval;
    }

    InputSendCadence {
        periodic_sends,
        residual_elapsed: residual_elapsed.min(interval),
    }
}

fn receive_authoritative_messages(ctx: &mut NetworkTickCtx<'_>) {
    ctx.received_authoritative = false;
    receive_join_snapshots(ctx);
    receive_world_deltas(ctx);
}

fn receive_join_snapshots(ctx: &mut NetworkTickCtx<'_>) {
    while let Some(bytes) = ctx
        .runtime
        .renet
        .receive_message(DefaultChannel::ReliableOrdered)
    {
        accumulate_rx_bytes(ctx, bytes.len() as u64);
        match decode::<JoinSnapshot>(&bytes) {
            Ok(snapshot) => {
                acknowledge_input_seq(
                    &mut ctx.runtime.pending_inputs,
                    snapshot.your_last_input_seq,
                );
                ctx.runtime.client_id = snapshot.you;
                log::info!(
                    "applied JoinSnapshot: you={} tick={} entities={}",
                    snapshot.you,
                    snapshot.tick,
                    snapshot.world.entities.len()
                );
                ctx.world.apply_snapshot(snapshot);
                ctx.received_authoritative = true;
            }
            Err(err) => {
                log::warn!(
                    "failed to decode JoinSnapshot: {err} ({} bytes)",
                    bytes.len()
                );
            }
        }
    }
}

fn receive_world_deltas(ctx: &mut NetworkTickCtx<'_>) {
    while let Some(bytes) = ctx
        .runtime
        .renet
        .receive_message(DefaultChannel::Unreliable)
    {
        accumulate_rx_bytes(ctx, bytes.len() as u64);
        match decode::<WorldDelta>(&bytes) {
            Ok(delta) => {
                acknowledge_input_seq(&mut ctx.runtime.pending_inputs, delta.your_last_input_seq);
                log::trace!(
                    "applied WorldDelta: tick={} upserts={} removed={} events={}",
                    delta.tick,
                    delta.upserts.len(),
                    delta.removed.len(),
                    delta.events.len()
                );
                ctx.world.apply_delta(delta);
                ctx.received_authoritative = true;
            }
            Err(err) => {
                log::warn!("failed to decode WorldDelta: {err} ({} bytes)", bytes.len());
            }
        }
    }
}

fn accumulate_rx_bytes(ctx: &mut NetworkTickCtx<'_>, len: u64) {
    if ctx.diag_enabled {
        ctx.traffic.rx_bytes_accum = ctx.traffic.rx_bytes_accum.saturating_add(len);
    }
}

fn reconcile_and_smooth_world(ctx: &mut NetworkTickCtx<'_>) {
    if ctx.received_authoritative {
        ctx.world
            .reconcile_local_prediction(&ctx.runtime.pending_inputs, ctx.runtime.clock_seconds);
    }

    let smoothing_dt = ctx.dt.as_secs_f32().min(MAX_SMOOTHING_DT_SECONDS);
    ctx.world.step_smoothing(smoothing_dt, ctx.movement_input);
}

fn update_network_diagnostics(ctx: &mut NetworkTickCtx<'_>) {
    if !ctx.diag_enabled {
        reset_traffic_meter(ctx.traffic);
        return;
    }

    ctx.traffic.elapsed += ctx.dt.as_secs_f32();
    if ctx.traffic.elapsed < APP_TRAFFIC_SAMPLE_SECONDS {
        return;
    }

    let sample_secs = ctx.traffic.elapsed.max(0.001);
    ctx.traffic.tx_rate_kib_s = ctx.traffic.tx_bytes_accum as f32 / 1024.0 / sample_secs;
    ctx.traffic.rx_rate_kib_s = ctx.traffic.rx_bytes_accum as f32 / 1024.0 / sample_secs;
    ctx.traffic.tx_bytes_accum = 0;
    ctx.traffic.rx_bytes_accum = 0;
    ctx.traffic.elapsed = 0.0;

    let network_info = ctx.runtime.renet.network_info();
    ctx.graphs.push(
        (network_info.rtt * 1000.0) as f32,
        (network_info.packet_loss * 100.0) as f32,
        ctx.traffic.tx_rate_kib_s,
        ctx.traffic.rx_rate_kib_s,
    );
}

fn reset_traffic_meter(traffic: &mut AppTrafficMeter) {
    traffic.tx_bytes_accum = 0;
    traffic.rx_bytes_accum = 0;
    traffic.elapsed = 0.0;
    traffic.tx_rate_kib_s = 0.0;
    traffic.rx_rate_kib_s = 0.0;
}

fn flush_transport_send(ctx: &mut NetworkTickCtx<'_>) {
    if ctx.runtime.renet.is_connected()
        && let Err(err) = ctx.runtime.transport.send_packets(&mut ctx.runtime.renet)
    {
        log::warn!(
            "transport send error (client_id={}): {err}",
            ctx.runtime.client_id
        );
    }
}

fn sample_local_input(
    keyboard: Res<ButtonInput<KeyCode>>,
    touches: Res<Touches>,
    primary_window: Query<&Window, With<bevy::window::PrimaryWindow>>,
    mut input_state: ResMut<LocalInputState>,
) {
    input_state.dir = read_movement_input(&keyboard, &touches, primary_window.iter().next());
}

fn fixed_prediction_tick(
    fixed_time: Res<bevy::time::Time<bevy::time::Fixed>>,
    input_state: Res<LocalInputState>,
    mut world: ResMut<WorldView>,
) {
    world.fixed_predict_local(fixed_time.delta_secs(), input_state.dir);
}

fn follow_local_player_camera(
    world: Res<WorldView>,
    mut cameras: Query<&mut Transform, With<MainCamera>>,
) {
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
    mut visuals: SceneVisualQuery<'_, '_>,
) {
    if !world.is_changed() {
        return;
    }

    upsert_world_visuals(&mut commands, &world, &mut render_index, &mut visuals);
    despawn_stale_visuals(&mut commands, &world, &mut render_index);
    sync_authoritative_ghost(
        &mut commands,
        time.delta_secs(),
        &world,
        &mut render_index,
        &mut visuals,
    );
}

fn upsert_world_visuals(
    commands: &mut Commands,
    world: &WorldView,
    render_index: &mut RenderIndex,
    visuals: &mut SceneVisualQuery<'_, '_>,
) {
    let current_render_count = render_index.by_id.len();
    let target_render_count = world.rendered.len();
    if current_render_count < target_render_count {
        render_index
            .by_id
            .reserve(target_render_count - current_render_count);
    }

    for entity_state in world.rendered.values() {
        let is_local_player = world.you == Some(entity_state.id);
        if let Some(entity) = render_index.by_id.get(&entity_state.id).copied() {
            if let Ok((mut visual, mut sprite, mut transform, _)) = visuals.get_mut(entity) {
                apply_visual_state(
                    &mut visual,
                    &mut sprite,
                    &mut transform,
                    entity_state,
                    is_local_player,
                );
            }
            continue;
        }

        let target = world_to_canvas(entity_state.pos);
        let spawned = commands
            .spawn((
                Sprite {
                    color: entity_color(entity_state, is_local_player),
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
}

fn apply_visual_state(
    visual: &mut VisualEntity,
    sprite: &mut Sprite,
    transform: &mut Transform,
    entity_state: &EntityState,
    is_local_player: bool,
) {
    let target = world_to_canvas(entity_state.pos);
    visual.target = target;
    visual.radius = entity_state.radius;

    let next_size = Vec2::splat(entity_state.radius * 2.0);
    if sprite.custom_size != Some(next_size) {
        sprite.custom_size = Some(next_size);
    }

    let next_color = entity_color(entity_state, is_local_player);
    if sprite.color != next_color {
        sprite.color = next_color;
    }

    if is_local_player {
        transform.translation.x = target.x;
        transform.translation.y = target.y;
    }
}

fn despawn_stale_visuals(
    commands: &mut Commands,
    world: &WorldView,
    render_index: &mut RenderIndex,
) {
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
}

fn sync_authoritative_ghost(
    commands: &mut Commands,
    dt_secs: f32,
    world: &WorldView,
    render_index: &mut RenderIndex,
    visuals: &mut SceneVisualQuery<'_, '_>,
) {
    let Some(authoritative_you) = world.you.and_then(|you| world.authoritative.get(&you)) else {
        if let Some(entity) = render_index.ghost_you.take() {
            commands.entity(entity).despawn();
        }
        return;
    };

    let target = world_to_canvas(authoritative_you.pos);
    if let Some(ghost_entity) = render_index.ghost_you {
        if let Ok((mut visual, mut sprite, mut transform, _)) = visuals.get_mut(ghost_entity) {
            let current = Vec2::new(transform.translation.x, transform.translation.y);
            let next = smooth_ghost_target(current, target, authoritative_you.vel, dt_secs);
            transform.translation.x = next.x;
            transform.translation.y = next.y;
            visual.target = next;
            visual.radius = authoritative_you.radius;

            let next_size = Vec2::splat(authoritative_you.radius * 2.0);
            if sprite.custom_size != Some(next_size) {
                sprite.custom_size = Some(next_size);
            }
            let next_color = ghost_color(authoritative_you);
            if sprite.color != next_color {
                sprite.color = next_color;
            }
        }
        return;
    }

    let spawned = commands
        .spawn((
            Sprite {
                color: ghost_color(authoritative_you),
                custom_size: Some(Vec2::splat(authoritative_you.radius * 2.0)),
                ..Default::default()
            },
            Transform::from_xyz(target.x, target.y, z_for_kind(EntityKind::Player) - 0.02),
            VisualEntity {
                target,
                radius: authoritative_you.radius,
            },
            GhostVisual,
        ))
        .id();
    render_index.ghost_you = Some(spawned);
}

fn smooth_ghost_target(current: Vec2, target: Vec2, velocity: [f32; 2], dt_secs: f32) -> Vec2 {
    let smoothing_dt = dt_secs.max(0.0).min(MAX_SMOOTHING_DT_SECONDS);
    let error = target - current;
    let velocity = Vec2::new(velocity[0], velocity[1]);
    if velocity.length_squared() > f32::EPSILON {
        let dir = velocity.normalize();
        let forward_error = dir * error.dot(dir);
        let lateral_error = error - forward_error;
        let forward_alpha = bounded_smoothing_factor(smoothing_dt, 10.0, 0.45);
        let lateral_alpha = bounded_smoothing_factor(smoothing_dt, 30.0, 0.75);
        return current + forward_error * forward_alpha + lateral_error * lateral_alpha;
    }

    let snap_alpha = bounded_smoothing_factor(smoothing_dt, 12.0, 0.55);
    current.lerp(target, snap_alpha)
}

fn animate_visuals(
    time: Res<Time>,
    mut query: Query<(&VisualEntity, &mut Transform), Without<GhostVisual>>,
) {
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

    if let Some(window) = primary_window
        && let Some(touch) = touches.iter().next()
    {
        let center = Vec2::new(window.width() * 0.5, window.height() * 0.5);
        let touch_pos = touch.position();
        let raw = Vec2::new(touch_pos.x - center.x, center.y - touch_pos.y);

        if raw.length_squared() > 64.0 {
            let normalized = raw.normalize();
            return [normalized.x, normalized.y];
        }
        return [0.0, 0.0];
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

fn acknowledge_input_seq(pending_inputs: &mut VecDeque<PendingInputCommand>, ack_seq: Option<u32>) {
    let Some(ack_seq) = ack_seq else {
        return;
    };

    while let Some(front) = pending_inputs.front() {
        if is_newer_input_seq(front.seq, ack_seq) {
            break;
        }
        pending_inputs.pop_front();
    }
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

fn bounded_smoothing_factor(dt_secs: f32, hz: f32, max_alpha: f32) -> f32 {
    smooth_step_factor(dt_secs.max(0.0).min(MAX_SMOOTHING_DT_SECONDS), hz).min(max_alpha)
}

fn normalize(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= f32::EPSILON {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

fn has_movement_input(move_dir: [f32; 2]) -> bool {
    move_dir[0].abs() > f32::EPSILON || move_dir[1].abs() > f32::EPSILON
}

fn should_edge_send_input(previous: [f32; 2], current: [f32; 2]) -> bool {
    let previous_moving = has_movement_input(previous);
    let current_moving = has_movement_input(current);

    if previous_moving != current_moving {
        return true;
    }
    if !current_moving {
        return false;
    }

    let previous_dir = normalize(previous);
    let current_dir = normalize(current);
    let dot = previous_dir[0] * current_dir[0] + previous_dir[1] * current_dir[1];
    dot < INPUT_DIRECTION_CHANGE_DOT_THRESHOLD
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_cmd(seq: u32) -> PendingInputCommand {
        PendingInputCommand {
            seq,
            move_dir: [0.0, 0.0],
            sent_at_seconds: seq as f32,
        }
    }

    fn approx_eq(left: f32, right: f32) {
        assert!((left - right).abs() < 1e-6, "left={left} right={right}");
    }

    #[test]
    fn should_edge_send_input() {
        assert!(super::should_edge_send_input([0.0, 0.0], [1.0, 0.0]));
        assert!(super::should_edge_send_input([1.0, 0.0], [0.0, 0.0]));
        assert!(!super::should_edge_send_input([1.0, 0.0], [1.0, 0.09]));
        assert!(super::should_edge_send_input([1.0, 0.0], [0.6, 0.8]));
    }

    #[test]
    fn acknowledge_input_seq() {
        let mut pending = VecDeque::from([pending_cmd(1), pending_cmd(2), pending_cmd(3)]);
        super::acknowledge_input_seq(&mut pending, Some(2));
        assert_eq!(
            pending.iter().map(|cmd| cmd.seq).collect::<Vec<_>>(),
            vec![3]
        );

        let mut wrap = VecDeque::from([
            pending_cmd(u32::MAX - 1),
            pending_cmd(u32::MAX),
            pending_cmd(0),
            pending_cmd(1),
        ]);
        super::acknowledge_input_seq(&mut wrap, Some(0));
        assert_eq!(wrap.iter().map(|cmd| cmd.seq).collect::<Vec<_>>(), vec![1]);

        let mut no_ack = VecDeque::from([pending_cmd(42)]);
        super::acknowledge_input_seq(&mut no_ack, None);
        assert_eq!(
            no_ack.iter().map(|cmd| cmd.seq).collect::<Vec<_>>(),
            vec![42]
        );
    }

    #[test]
    fn sample_at_column() {
        let empty = VecDeque::new();
        assert_eq!(super::sample_at_column(&empty, 0), 0.0);

        let short = VecDeque::from([2.0, 4.0]);
        assert_eq!(super::sample_at_column(&short, 0), 0.0);
        assert_eq!(super::sample_at_column(&short, NET_GRAPH_SAMPLES - 3), 0.0);
        assert_eq!(super::sample_at_column(&short, NET_GRAPH_SAMPLES - 2), 2.0);
        assert_eq!(super::sample_at_column(&short, NET_GRAPH_SAMPLES - 1), 4.0);

        let full: VecDeque<f32> = (0..NET_GRAPH_SAMPLES).map(|idx| idx as f32).collect();
        assert_eq!(super::sample_at_column(&full, 0), 0.0);
        assert_eq!(
            super::sample_at_column(&full, NET_GRAPH_SAMPLES - 1),
            (NET_GRAPH_SAMPLES - 1) as f32
        );
    }

    #[test]
    fn send_cadence_helper_handles_spike() {
        let interval = INPUT_SEND_INTERVAL_SECONDS;
        let cadence = super::plan_periodic_input_sends(interval * 8.5, interval, 3, false);
        assert_eq!(cadence.periodic_sends, 3);
        approx_eq(cadence.residual_elapsed, interval);

        let edge_cadence = super::plan_periodic_input_sends(interval * 1.25, interval, 4, true);
        assert_eq!(edge_cadence.periodic_sends, 0);
        approx_eq(edge_cadence.residual_elapsed, interval * 0.25);
    }
}
