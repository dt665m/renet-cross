use std::{
    collections::{HashMap, HashSet},
    f32::consts::TAU,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use bevy::{
    app::ScheduleRunnerPlugin,
    log::LogPlugin,
    prelude::*,
    window::{Window, WindowPlugin},
};
use clap::{Parser, ValueEnum};
use renet::{ConnectionConfig, DefaultChannel, RenetServer, ServerEvent};
use renet_server::{
    BootstrapConfig, BootstrapService, MixedServerTransport, MixedTransportBuilder,
    MonotonicClientIdAllocator, ServerAuthentication, UnsecureDevAuthPolicy,
};
use shared::{
    BASE_PLAYER_MASS, BASE_PLAYER_SPEED, ClientInput, EntityKind, EntityState, FIXED_DT_SECONDS,
    JoinSnapshot, PELLET_MASS, PLAYER_CONSUME_RATIO, RESPAWN_TICKS, TARGET_PELLET_COUNT,
    TICK_RATE_HZ, WORLD_HEIGHT, WORLD_WIDTH, WorldDelta, WorldEvent, WorldState, decode, encode,
    is_newer_input_seq,
};

mod http_api;
use http_api::{AppState, HttpTlsConfig, spawn_http_server_thread};

const PROTOCOL_ID: u64 = 7;
const SERVER_TICK_HZ: u64 = TICK_RATE_HZ as u64;
const HEADLESS_RUN_HZ: f64 = 240.0;
const WORLD_BORDER_THICKNESS: f32 = 14.0;
const CAMERA_MOVE_SPEED: f32 = 950.0;
const CAMERA_BOOST_MULTIPLIER: f32 = 2.0;
const INITIAL_ENTITY_ID: u64 = 1_000_000;
const INPUT_STALE_TIMEOUT_TICKS: u32 = 3;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ServerMode {
    Headless,
    Ui,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "server")]
struct ServerArgs {
    #[arg(long, env = "NET_SERVER_MODE", value_enum, default_value_t = ServerMode::Ui)]
    mode: ServerMode,
    #[arg(long, env = "NET_UDP_BIND", default_value = "0.0.0.0:5000")]
    udp_bind: SocketAddr,
    #[arg(long, env = "NET_WEBRTC_BIND", default_value = "0.0.0.0:5001")]
    webrtc_bind: SocketAddr,
    #[arg(long, env = "NET_HTTP_BIND")]
    http_bind: Option<SocketAddr>,
    #[arg(long, env = "NET_HTTP_TLS_CERT")]
    http_tls_cert: Option<PathBuf>,
    #[arg(long, env = "NET_HTTP_TLS_KEY")]
    http_tls_key: Option<PathBuf>,
    #[arg(long, env = "NET_PUBLIC_UDP_ADDR", default_value = "127.0.0.1:5000")]
    public_udp_addr: SocketAddr,
    #[arg(long, env = "NET_PUBLIC_WEBRTC_ADDR", default_value = "127.0.0.1:5001")]
    public_webrtc_addr: SocketAddr,
    #[arg(long, env = "NET_PUBLIC_HTTP_BASE")]
    public_http_base: Option<String>,
    #[arg(long, env = "NET_CLIENT_DIST", default_value = "examples/client/dist")]
    client_dist: PathBuf,
}

#[derive(Clone)]
struct SharedNet {
    bootstrap: Arc<BootstrapService<MonotonicClientIdAllocator, UnsecureDevAuthPolicy>>,
    server: Arc<Mutex<RenetServer>>,
    transport: Arc<Mutex<MixedServerTransport>>,
}

impl SharedNet {
    fn new(
        bootstrap: Arc<BootstrapService<MonotonicClientIdAllocator, UnsecureDevAuthPolicy>>,
        server: Arc<Mutex<RenetServer>>,
        transport: Arc<Mutex<MixedServerTransport>>,
    ) -> Self {
        Self {
            bootstrap,
            server,
            transport,
        }
    }

    fn with_server<R>(&self, f: impl FnOnce(&mut RenetServer) -> R) -> Option<R> {
        let Ok(mut server) = self.server.lock() else {
            return None;
        };
        Some(f(&mut server))
    }

    fn with_server_and_transport<R>(
        &self,
        f: impl FnOnce(&mut RenetServer, &mut MixedServerTransport) -> R,
    ) -> Option<R> {
        let Ok(mut server) = self.server.lock() else {
            return None;
        };
        let Ok(mut transport) = self.transport.lock() else {
            return None;
        };
        Some(f(&mut server, &mut transport))
    }
}

#[derive(Resource, Clone)]
struct NetRuntime {
    net: SharedNet,
}

#[derive(Resource, Default)]
struct SimTick {
    tick: u32,
}

#[derive(Resource, Default)]
struct ServerTickCounter(u64);

#[derive(Debug, Clone)]
struct RespawnEntry {
    client_id: u64,
    at_tick: u32,
}

#[derive(Resource, Debug)]
struct AuthoritativeState {
    next_entity_id: u64,
    rng_state: u64,
    inputs: HashMap<u64, ClientInput>,
    last_input_tick: HashMap<u64, u32>,
    last_input_seq: HashMap<u64, u32>,
    player_colors: HashMap<u64, [f32; 3]>,
    respawn_queue: Vec<RespawnEntry>,
    snapshot_cache: HashMap<u64, EntityState>,
    pending_connects: Vec<u64>,
    pending_disconnects: Vec<u64>,
    pending_join_snapshots: Vec<u64>,
}

impl AuthoritativeState {
    fn new(seed: u64) -> Self {
        Self {
            next_entity_id: INITIAL_ENTITY_ID,
            rng_state: seed.max(1),
            inputs: HashMap::new(),
            last_input_tick: HashMap::new(),
            last_input_seq: HashMap::new(),
            player_colors: HashMap::new(),
            respawn_queue: Vec::new(),
            snapshot_cache: HashMap::new(),
            pending_connects: Vec::new(),
            pending_disconnects: Vec::new(),
            pending_join_snapshots: Vec::new(),
        }
    }
}

#[derive(Component, Debug, Clone)]
struct AuthoritativeEntity {
    id: u64,
    kind: EntityKind,
    pos: Vec2,
    vel: Vec2,
    mass: f32,
    radius: f32,
    color: [f32; 3],
}

impl AuthoritativeEntity {
    fn to_entity_state(&self) -> EntityState {
        EntityState {
            id: self.id,
            kind: self.kind,
            pos: [self.pos.x, self.pos.y],
            vel: [self.vel.x, self.vel.y],
            mass: self.mass,
            radius: self.radius,
            color: self.color,
        }
    }
}

#[derive(Component, Debug, Clone, Copy)]
struct PlayerOwned {
    client_id: u64,
}

#[derive(Component, Debug, Clone, Copy)]
struct PelletTag;

#[derive(Resource, Default)]
struct SceneIndex {
    by_id: HashMap<u64, Entity>,
}

#[derive(Component)]
struct ServerDebugCamera;

#[derive(Component)]
struct WorldVisualEntity;

#[derive(Component)]
struct NetPanelText;

fn default_http_bind(tls_enabled: bool) -> SocketAddr {
    if tls_enabled {
        SocketAddr::from(([0, 0, 0, 0], 443))
    } else {
        SocketAddr::from(([0, 0, 0, 0], 8080))
    }
}

fn default_public_http_base(http_bind: SocketAddr, tls_enabled: bool) -> String {
    let scheme = if tls_enabled { "https" } else { "http" };
    let default_host = "127.0.0.1";
    let default_port = if tls_enabled { 443 } else { 80 };

    if http_bind.port() == default_port {
        format!("{scheme}://{default_host}")
    } else {
        format!("{scheme}://{default_host}:{}", http_bind.port())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = ServerArgs::parse();
    let ServerArgs {
        mode,
        udp_bind,
        webrtc_bind,
        http_bind,
        http_tls_cert,
        http_tls_key,
        public_udp_addr,
        public_webrtc_addr,
        public_http_base,
        client_dist,
    } = args;

    let http_tls = match (http_tls_cert, http_tls_key) {
        (Some(cert_path), Some(key_path)) => Some(HttpTlsConfig {
            cert_path,
            key_path,
        }),
        (None, None) => None,
        (Some(_), None) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "NET_HTTP_TLS_CERT was set but NET_HTTP_TLS_KEY is missing",
            )
            .into());
        }
        (None, Some(_)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "NET_HTTP_TLS_KEY was set but NET_HTTP_TLS_CERT is missing",
            )
            .into());
        }
    };
    let tls_enabled = http_tls.is_some();
    let http_bind = http_bind.unwrap_or_else(|| default_http_bind(tls_enabled));
    let mut public_http_base =
        public_http_base.unwrap_or_else(|| default_public_http_base(http_bind, tls_enabled));
    if tls_enabled && public_http_base.starts_with("http://") {
        public_http_base = format!("https://{}", public_http_base.trim_start_matches("http://"));
    }

    log::info!(
        "startup config: mode={mode:?} http_bind={http_bind} http_tls={} udp_bind={udp_bind} webrtc_bind={webrtc_bind} tick_hz={SERVER_TICK_HZ} public_http_base={public_http_base} public_udp_addr={public_udp_addr} public_webrtc_addr={public_webrtc_addr} client_dist={}",
        http_tls.is_some(),
        client_dist.display()
    );

    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
    let rng_seed = now.as_nanos() as u64;

    let server = Arc::new(Mutex::new(RenetServer::new(ConnectionConfig::default())));
    let transport = Arc::new(Mutex::new(
        MixedTransportBuilder::new(PROTOCOL_ID)
            .udp_bind(udp_bind)
            .webrtc_bind(webrtc_bind)
            .public_udp_addr(public_udp_addr)
            .public_webrtc_addr(public_webrtc_addr)
            .max_clients(512)
            .authentication(ServerAuthentication::Unsecure)
            .build()?,
    ));
    let bootstrap = Arc::new(BootstrapService::new(
        BootstrapConfig {
            session_ttl: Duration::from_secs(120),
            public_udp_addr,
            public_webrtc_addr,
            public_http_base: public_http_base.clone(),
        },
        MonotonicClientIdAllocator::new(1),
        UnsecureDevAuthPolicy,
    ));
    let net = SharedNet::new(bootstrap.clone(), server, transport.clone());

    let app_state = AppState::new(bootstrap, transport, public_webrtc_addr);

    let _http_thread = spawn_http_server_thread(app_state, http_bind, client_dist, http_tls);
    run_bevy_server(mode, NetRuntime { net }, rng_seed);

    Ok(())
}

fn run_bevy_server(mode: ServerMode, runtime: NetRuntime, rng_seed: u64) {
    let mut app = App::new();

    match mode {
        ServerMode::Headless => {
            app.add_plugins(
                MinimalPlugins
                    .set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
                        1.0 / HEADLESS_RUN_HZ,
                    )))
                    .disable::<LogPlugin>(),
            );
        }
        ServerMode::Ui => {
            app.add_plugins(
                DefaultPlugins
                    .set(WindowPlugin {
                        primary_window: Some(Window {
                            title: "Authoritative Server (UI Debug)".to_owned(),
                            resolution: (1320_u32, 860_u32).into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .disable::<LogPlugin>(),
            )
            .insert_resource(ClearColor(Color::srgb(0.02, 0.025, 0.04)))
            .insert_resource(SceneIndex::default())
            .add_systems(Startup, setup_ui_scene)
            .add_systems(
                Update,
                (
                    server_camera_controls,
                    sync_world_to_scene,
                    update_network_panel,
                ),
            );
        }
    }

    app.insert_resource(Time::<Fixed>::from_hz(TICK_RATE_HZ as f64))
        .insert_resource(runtime)
        .insert_resource(SimTick::default())
        .insert_resource(ServerTickCounter::default())
        .insert_resource(AuthoritativeState::new(rng_seed))
        .add_systems(Startup, bootstrap_authoritative_world)
        .add_systems(Update, network_transport_update)
        .add_systems(FixedUpdate, fixed_server_tick)
        .add_systems(PostUpdate, flush_transport_packets)
        .run();
}

fn bootstrap_authoritative_world(world: &mut World) {
    maintain_pellet_budget(world);
}

fn network_transport_update(
    time: Res<Time>,
    runtime: Res<NetRuntime>,
    mut auth_state: ResMut<AuthoritativeState>,
) {
    let dt = time.delta();
    let _ = runtime.net.with_server_and_transport(|server, transport| {
        server.update(dt);
        if let Err(err) = transport.update(dt, server) {
            log::warn!("transport update failed: {err}");
        }

        while let Some(event) = server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    if let Err(err) = runtime.net.bootstrap.on_client_connected(client_id) {
                        log::warn!("disconnecting unauthorized client_id={client_id}: {err}");
                        server.disconnect(client_id);
                        continue;
                    }

                    if !auth_state.pending_connects.contains(&client_id) {
                        auth_state.pending_connects.push(client_id);
                    }
                    log::info!("client connected (pending authoritative spawn): {client_id}");
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    runtime.net.bootstrap.on_client_disconnected(client_id);

                    if !auth_state.pending_disconnects.contains(&client_id) {
                        auth_state.pending_disconnects.push(client_id);
                    }
                    log::info!("client disconnected: {client_id} ({reason})");
                }
            }
        }
    });
}

fn fixed_server_tick(world: &mut World) {
    let runtime = world.resource::<NetRuntime>().clone();
    let _ = runtime.net.with_server(|server| {
        let connected_clients: HashSet<u64> = server.clients_id().into_iter().collect();
        let input_tick = world.resource::<SimTick>().tick;

        let pending_disconnects = take_pending_disconnects(world);
        for client_id in pending_disconnects {
            handle_disconnect(world, client_id);
        }

        let pending_connects = take_pending_connects(world);
        for client_id in pending_connects {
            if !connected_clients.contains(&client_id) {
                continue;
            }
            if player_exists(world, client_id) {
                continue;
            }
            spawn_player(world, client_id);
            queue_join_snapshot(world, client_id);
        }

        for client_id in server.clients_id() {
            while let Some(bytes) =
                server.receive_message(client_id, DefaultChannel::ReliableOrdered)
            {
                process_client_input_message(world, client_id, &bytes, input_tick);
            }
            while let Some(bytes) = server.receive_message(client_id, DefaultChannel::Unreliable) {
                process_client_input_message(world, client_id, &bytes, input_tick);
            }
        }

        {
            let mut sim_tick = world.resource_mut::<SimTick>();
            sim_tick.tick = sim_tick.tick.wrapping_add(1);
        }
        let current_tick = world.resource::<SimTick>().tick;

        let mut events = Vec::new();
        process_respawns(world, current_tick, &connected_clients, &mut events);
        apply_player_movement(world, current_tick);
        consume_pellets(world, &mut events);
        consume_players(world, current_tick, &mut events);
        maintain_pellet_budget(world);

        let delta = build_world_delta(world, current_tick, events);
        if !(delta.upserts.is_empty() && delta.removed.is_empty() && delta.events.is_empty()) {
            send_world_delta_to_clients(world, server, &delta);
        }

        send_pending_join_snapshots(world, server, current_tick, &connected_clients);

        {
            let mut tick_counter = world.resource_mut::<ServerTickCounter>();
            tick_counter.0 = tick_counter.0.wrapping_add(1);
            if tick_counter.0.is_multiple_of(100) {
                log::debug!(
                    "authoritative tick={} sim_tick={} connected_clients={}",
                    tick_counter.0,
                    current_tick,
                    connected_clients.len()
                );
            }
        }
    });
}

fn take_pending_disconnects(world: &mut World) -> Vec<u64> {
    let mut pending = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        std::mem::take(&mut state.pending_disconnects)
    };
    pending.sort_unstable();
    pending.dedup();
    pending
}

fn process_client_input_message(world: &mut World, client_id: u64, bytes: &[u8], input_tick: u32) {
    match decode::<ClientInput>(bytes) {
        Ok(input) => {
            let moving =
                input.move_dir[0].abs() > f32::EPSILON || input.move_dir[1].abs() > f32::EPSILON;
            if moving && input.seq % 5 == 0 {
                log::debug!(
                    "server input client_id={} seq={} dir=[{:.2}, {:.2}]",
                    client_id,
                    input.seq,
                    input.move_dir[0],
                    input.move_dir[1]
                );
            }

            let seq = input.seq;
            let mut auth_state = world.resource_mut::<AuthoritativeState>();
            let is_newer = auth_state
                .last_input_seq
                .get(&client_id)
                .copied()
                .is_none_or(|last| is_newer_input_seq(seq, last));

            if is_newer {
                auth_state.last_input_seq.insert(client_id, seq);
                auth_state.last_input_tick.insert(client_id, input_tick);
                auth_state.inputs.insert(client_id, input);
            } else {
                log::trace!(
                    "ignoring stale out-of-order input from client_id={} seq={} (latest={})",
                    client_id,
                    seq,
                    auth_state
                        .last_input_seq
                        .get(&client_id)
                        .copied()
                        .unwrap_or(seq)
                );
            }
        }
        Err(err) => {
            log::debug!(
                "dropping invalid ClientInput from client_id={client_id}: {err} ({} bytes)",
                bytes.len()
            );
        }
    }
}

fn take_pending_connects(world: &mut World) -> Vec<u64> {
    let mut pending = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        std::mem::take(&mut state.pending_connects)
    };
    pending.sort_unstable();
    pending.dedup();
    pending
}

fn queue_join_snapshot(world: &mut World, client_id: u64) {
    let mut state = world.resource_mut::<AuthoritativeState>();
    if !state.pending_join_snapshots.contains(&client_id) {
        state.pending_join_snapshots.push(client_id);
    }
}

fn take_pending_join_snapshots(world: &mut World) -> Vec<u64> {
    let mut pending = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        std::mem::take(&mut state.pending_join_snapshots)
    };
    pending.sort_unstable();
    pending.dedup();
    pending
}

fn handle_disconnect(world: &mut World, client_id: u64) {
    despawn_player_entities(world, client_id);
    let mut state = world.resource_mut::<AuthoritativeState>();
    state.inputs.remove(&client_id);
    state.last_input_tick.remove(&client_id);
    state.last_input_seq.remove(&client_id);
    state.player_colors.remove(&client_id);
    state
        .respawn_queue
        .retain(|entry| entry.client_id != client_id);
    state.pending_join_snapshots.retain(|id| *id != client_id);
}

fn despawn_player_entities(world: &mut World, client_id: u64) {
    let entities: Vec<Entity> = {
        let mut query = world.query::<(Entity, &PlayerOwned)>();
        query
            .iter(world)
            .filter_map(|(entity, owner)| (owner.client_id == client_id).then_some(entity))
            .collect()
    };

    for entity in entities {
        let _ = world.despawn(entity);
    }
}

fn player_exists(world: &mut World, client_id: u64) -> bool {
    let mut query = world.query::<&PlayerOwned>();
    query.iter(world).any(|owner| owner.client_id == client_id)
}

fn spawn_player(world: &mut World, client_id: u64) {
    let color = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        if let Some(color) = state.player_colors.get(&client_id).copied() {
            color
        } else {
            let hue = rand_range(&mut state.rng_state, 0.0, 360.0);
            let color = hsv_to_rgb(hue, 0.72, 0.88);
            state.player_colors.insert(client_id, color);
            color
        }
    };

    let mut spawn = spawn_point_from_id(client_id);
    spawn[0] = clamp_axis(spawn[0], WORLD_WIDTH);
    spawn[1] = clamp_axis(spawn[1], WORLD_HEIGHT);

    world.spawn((
        AuthoritativeEntity {
            id: client_id,
            kind: EntityKind::Player,
            pos: Vec2::new(spawn[0], spawn[1]),
            vel: Vec2::ZERO,
            mass: BASE_PLAYER_MASS,
            radius: radius_from_mass(BASE_PLAYER_MASS),
            color,
        },
        PlayerOwned { client_id },
    ));
}

fn process_respawns(
    world: &mut World,
    current_tick: u32,
    connected_clients: &HashSet<u64>,
    events: &mut Vec<WorldEvent>,
) {
    let due_client_ids = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        let mut due = Vec::new();
        let mut pending = Vec::new();
        for entry in std::mem::take(&mut state.respawn_queue) {
            if entry.at_tick <= current_tick {
                due.push(entry.client_id);
            } else {
                pending.push(entry);
            }
        }
        state.respawn_queue = pending;
        due
    };

    for client_id in due_client_ids {
        if !connected_clients.contains(&client_id) {
            continue;
        }
        if player_exists(world, client_id) {
            continue;
        }
        spawn_player(world, client_id);
        events.push(WorldEvent::PlayerRespawned {
            player_id: client_id,
        });
    }
}

fn apply_player_movement(world: &mut World, current_tick: u32) {
    let (inputs, last_input_tick) = {
        let state = world.resource::<AuthoritativeState>();
        (state.inputs.clone(), state.last_input_tick.clone())
    };

    let mut query = world.query::<(&mut AuthoritativeEntity, &PlayerOwned)>();
    for (mut player, owner) in query.iter_mut(world) {
        let dir = if let Some(input) = inputs.get(&owner.client_id) {
            let is_fresh = last_input_tick
                .get(&owner.client_id)
                .map(|tick| current_tick.wrapping_sub(*tick) <= INPUT_STALE_TIMEOUT_TICKS)
                .unwrap_or(false);
            if is_fresh {
                normalize(input.move_dir)
            } else {
                [0.0, 0.0]
            }
        } else {
            [0.0, 0.0]
        };

        let speed = BASE_PLAYER_SPEED / (1.0 + player.mass * 0.015);
        player.vel = Vec2::new(dir[0] * speed, dir[1] * speed);
        player.pos.x = clamp_axis(player.pos.x + player.vel.x * FIXED_DT_SECONDS, WORLD_WIDTH);
        player.pos.y = clamp_axis(player.pos.y + player.vel.y * FIXED_DT_SECONDS, WORLD_HEIGHT);
    }
}

#[derive(Debug, Clone, Copy)]
struct PlayerSnapshot {
    entity: Entity,
    client_id: u64,
    pos: [f32; 2],
    mass: f32,
    radius: f32,
}

#[derive(Debug, Clone, Copy)]
struct PelletSnapshot {
    entity: Entity,
    id: u64,
    pos: [f32; 2],
    radius: f32,
}

fn collect_player_snapshots(world: &mut World) -> Vec<PlayerSnapshot> {
    let mut query = world.query::<(Entity, &AuthoritativeEntity, &PlayerOwned)>();
    let mut players = Vec::new();
    for (entity, state, owner) in query.iter(world) {
        players.push(PlayerSnapshot {
            entity,
            client_id: owner.client_id,
            pos: [state.pos.x, state.pos.y],
            mass: state.mass,
            radius: state.radius,
        });
    }
    players.sort_unstable_by_key(|player| player.client_id);
    players
}

fn collect_pellet_snapshots(world: &mut World) -> Vec<PelletSnapshot> {
    let mut query = world.query::<(Entity, &AuthoritativeEntity, &PelletTag)>();
    let mut pellets = Vec::new();
    for (entity, state, _) in query.iter(world) {
        pellets.push(PelletSnapshot {
            entity,
            id: state.id,
            pos: [state.pos.x, state.pos.y],
            radius: state.radius,
        });
    }
    pellets.sort_unstable_by_key(|pellet| pellet.id);
    pellets
}

fn consume_pellets(world: &mut World, events: &mut Vec<WorldEvent>) {
    let players = collect_player_snapshots(world);
    let pellets = collect_pellet_snapshots(world);
    if players.is_empty() || pellets.is_empty() {
        return;
    }

    let mut available: HashMap<u64, PelletSnapshot> = pellets
        .into_iter()
        .map(|pellet| (pellet.id, pellet))
        .collect();
    let mut mass_gain_by_player: HashMap<u64, f32> = HashMap::new();
    let mut consumed_records: Vec<(u64, PelletSnapshot)> = Vec::new();

    for player in &players {
        let mut consumed_now = Vec::new();
        for pellet in available.values() {
            let dist_sq = distance_sq(player.pos, pellet.pos);
            let max_dist = player.radius + pellet.radius;
            if dist_sq <= max_dist * max_dist {
                consumed_now.push(pellet.id);
            }
        }

        consumed_now.sort_unstable();
        for pellet_id in consumed_now {
            if let Some(pellet) = available.remove(&pellet_id) {
                *mass_gain_by_player.entry(player.client_id).or_insert(0.0) += PELLET_MASS;
                consumed_records.push((player.client_id, pellet));
            }
        }
    }

    if consumed_records.is_empty() {
        return;
    }

    {
        let mut query = world.query::<(&mut AuthoritativeEntity, &PlayerOwned)>();
        for (mut player, owner) in query.iter_mut(world) {
            if let Some(mass_gain) = mass_gain_by_player.get(&owner.client_id).copied() {
                player.mass += mass_gain;
                player.radius = radius_from_mass(player.mass);
            }
        }
    }

    for (player_id, pellet) in consumed_records {
        let _ = world.despawn(pellet.entity);
        events.push(WorldEvent::PelletConsumed {
            player_id,
            pellet_id: pellet.id,
        });
    }
}

fn consume_players(world: &mut World, current_tick: u32, events: &mut Vec<WorldEvent>) {
    let players = collect_player_snapshots(world);
    if players.len() < 2 {
        return;
    }

    let mut dead: HashSet<u64> = HashSet::new();
    let mut consumer_mass_gains: HashMap<u64, f32> = HashMap::new();
    let mut consumed_players: Vec<PlayerSnapshot> = Vec::new();

    for i in 0..players.len() {
        for j in (i + 1)..players.len() {
            let a = players[i];
            let b = players[j];

            if dead.contains(&a.client_id) || dead.contains(&b.client_id) {
                continue;
            }

            let dist_sq = distance_sq(a.pos, b.pos);
            let max_dist = (a.radius + b.radius) * 0.8;
            if dist_sq > max_dist * max_dist {
                continue;
            }

            let (consumer, consumed, mass_gain) = if a.mass >= b.mass * PLAYER_CONSUME_RATIO {
                (a.client_id, b, b.mass * 0.8)
            } else if b.mass >= a.mass * PLAYER_CONSUME_RATIO {
                (b.client_id, a, a.mass * 0.8)
            } else {
                continue;
            };

            dead.insert(consumed.client_id);
            *consumer_mass_gains.entry(consumer).or_insert(0.0) += mass_gain;
            consumed_players.push(consumed);
            events.push(WorldEvent::PlayerConsumed {
                consumer_id: consumer,
                consumed_id: consumed.client_id,
            });
        }
    }

    if consumed_players.is_empty() {
        return;
    }

    {
        let mut query = world.query::<(&mut AuthoritativeEntity, &PlayerOwned)>();
        for (mut player, owner) in query.iter_mut(world) {
            if let Some(mass_gain) = consumer_mass_gains.get(&owner.client_id).copied() {
                player.mass += mass_gain;
                player.radius = radius_from_mass(player.mass);
            }
        }
    }

    {
        let mut state = world.resource_mut::<AuthoritativeState>();
        for consumed in &consumed_players {
            state.inputs.remove(&consumed.client_id);
            state.last_input_tick.remove(&consumed.client_id);
            state.respawn_queue.push(RespawnEntry {
                client_id: consumed.client_id,
                at_tick: current_tick + RESPAWN_TICKS,
            });
        }
    }

    for consumed in consumed_players {
        let _ = world.despawn(consumed.entity);
    }
}

fn maintain_pellet_budget(world: &mut World) {
    let pellet_count = {
        let mut query = world.query::<&PelletTag>();
        query.iter(world).count()
    };

    if pellet_count >= TARGET_PELLET_COUNT {
        return;
    }

    let missing = TARGET_PELLET_COUNT - pellet_count;
    let pellets_to_spawn: Vec<AuthoritativeEntity> = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        let mut pellets = Vec::with_capacity(missing);
        for _ in 0..missing {
            let id = state.next_entity_id;
            state.next_entity_id = state.next_entity_id.wrapping_add(1);
            let x = rand_range(&mut state.rng_state, 0.0, WORLD_WIDTH);
            let y = rand_range(&mut state.rng_state, 0.0, WORLD_HEIGHT);

            pellets.push(AuthoritativeEntity {
                id,
                kind: EntityKind::Pellet,
                pos: Vec2::new(x, y),
                vel: Vec2::ZERO,
                mass: PELLET_MASS,
                radius: radius_from_mass(PELLET_MASS),
                color: [0.96, 0.76, 0.31],
            });
        }
        pellets
    };

    for pellet in pellets_to_spawn {
        world.spawn((pellet, PelletTag));
    }
}

fn collect_current_entity_map(world: &mut World) -> HashMap<u64, EntityState> {
    let entity_count = {
        let mut query = world.query::<&AuthoritativeEntity>();
        query.iter(world).len()
    };

    let mut query = world.query::<&AuthoritativeEntity>();
    let mut current = HashMap::with_capacity(entity_count);
    for entity in query.iter(world) {
        current.insert(entity.id, entity.to_entity_state());
    }
    current
}

fn collect_world_entities_sorted(world: &mut World) -> Vec<EntityState> {
    let entity_count = {
        let mut query = world.query::<&AuthoritativeEntity>();
        query.iter(world).len()
    };

    let mut query = world.query::<&AuthoritativeEntity>();
    let mut entities = Vec::with_capacity(entity_count);
    for entity in query.iter(world) {
        entities.push(entity.to_entity_state());
    }
    entities.sort_unstable_by_key(|entity| entity.id);
    entities
}

fn build_world_delta(world: &mut World, tick: u32, events: Vec<WorldEvent>) -> WorldDelta {
    let current = collect_current_entity_map(world);

    let (mut upserts, mut removed) = {
        let mut state = world.resource_mut::<AuthoritativeState>();
        let snapshot_len = state.snapshot_cache.len();

        let mut upserts = Vec::with_capacity(current.len());
        for (id, entity) in &current {
            let changed = state.snapshot_cache.get(id) != Some(entity);
            if changed {
                upserts.push(entity.clone());
            }
        }

        let mut removed = Vec::with_capacity(snapshot_len.saturating_sub(current.len()));
        for old_id in state.snapshot_cache.keys() {
            if !current.contains_key(old_id) {
                removed.push(*old_id);
            }
        }

        state.snapshot_cache = current;
        (upserts, removed)
    };

    upserts.sort_unstable_by_key(|entity| entity.id);
    removed.sort_unstable();

    WorldDelta {
        tick,
        your_last_input_seq: None,
        upserts,
        removed,
        events,
    }
}

fn send_world_delta_to_clients(world: &mut World, server: &mut RenetServer, delta: &WorldDelta) {
    let input_acks = world
        .resource::<AuthoritativeState>()
        .last_input_seq
        .clone();
    for client_id in server.clients_id() {
        let mut per_client_delta = delta.clone();
        per_client_delta.your_last_input_seq = input_acks.get(&client_id).copied();
        server.send_message(
            client_id,
            DefaultChannel::Unreliable,
            encode(&per_client_delta),
        );
    }
}

fn send_pending_join_snapshots(
    world: &mut World,
    server: &mut RenetServer,
    tick: u32,
    connected_clients: &HashSet<u64>,
) {
    let pending = take_pending_join_snapshots(world);
    if pending.is_empty() {
        return;
    }

    let world_state = WorldState {
        tick,
        entities: collect_world_entities_sorted(world),
    };
    let input_acks = world
        .resource::<AuthoritativeState>()
        .last_input_seq
        .clone();

    for client_id in pending {
        if !connected_clients.contains(&client_id) {
            continue;
        }
        if !player_exists(world, client_id) {
            continue;
        }

        let snapshot = JoinSnapshot {
            you: client_id,
            tick,
            your_last_input_seq: input_acks.get(&client_id).copied(),
            world: world_state.clone(),
        };
        server.send_message(
            client_id,
            DefaultChannel::ReliableOrdered,
            encode(&snapshot),
        );
    }
}

fn flush_transport_packets(runtime: Res<NetRuntime>) {
    let _ = runtime
        .net
        .with_server_and_transport(|server, transport| transport.send_packets(server));
}

fn setup_ui_scene(mut commands: Commands) {
    commands.spawn((
        Camera2d,
        ServerDebugCamera,
        Transform::from_xyz(0.0, 0.0, 1000.0),
    ));
    spawn_world_borders(&mut commands);
    spawn_network_panel(&mut commands);
}

fn spawn_world_borders(commands: &mut Commands) {
    let half_w = WORLD_WIDTH * 0.5;
    let half_h = WORLD_HEIGHT * 0.5;
    let t = WORLD_BORDER_THICKNESS;
    let color = Color::srgba(0.96, 0.96, 1.0, 0.38);

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

fn spawn_network_panel(commands: &mut Commands) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(14.0),
            top: Val::Px(14.0),
            width: Val::Px(520.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(10.0)),
            ..Default::default()
        },
        BackgroundColor(Color::srgba(0.08, 0.1, 0.15, 0.86)),
        children![(
            Text::new("server bootstrapping..."),
            TextFont {
                font_size: 14.0,
                ..Default::default()
            },
            TextColor(Color::srgb(0.9, 0.95, 1.0)),
            NetPanelText,
        )],
    ));
}

fn server_camera_controls(
    time: Res<Time>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<&mut Transform, With<ServerDebugCamera>>,
) {
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };

    let mut dir = Vec2::ZERO;
    if keyboard.pressed(KeyCode::KeyW) || keyboard.pressed(KeyCode::ArrowUp) {
        dir.y += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) || keyboard.pressed(KeyCode::ArrowDown) {
        dir.y -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) || keyboard.pressed(KeyCode::ArrowLeft) {
        dir.x -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) || keyboard.pressed(KeyCode::ArrowRight) {
        dir.x += 1.0;
    }

    if dir.length_squared() > 1.0 {
        dir = dir.normalize();
    }

    let mut speed = CAMERA_MOVE_SPEED;
    if keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight) {
        speed *= CAMERA_BOOST_MULTIPLIER;
    }

    transform.translation.x += dir.x * speed * time.delta_secs();
    transform.translation.y += dir.y * speed * time.delta_secs();

    let x_limit = WORLD_WIDTH * 0.5;
    let y_limit = WORLD_HEIGHT * 0.5;
    transform.translation.x = transform.translation.x.clamp(-x_limit, x_limit);
    transform.translation.y = transform.translation.y.clamp(-y_limit, y_limit);
}

fn sync_world_to_scene(
    mut commands: Commands,
    auth_entities: Query<&AuthoritativeEntity>,
    mut index: ResMut<SceneIndex>,
    mut visuals: Query<(&mut Transform, &mut Sprite), With<WorldVisualEntity>>,
) {
    let mut world_entities: Vec<EntityState> = auth_entities
        .iter()
        .map(|entity| entity.to_entity_state())
        .collect();
    world_entities.sort_unstable_by_key(|entity| entity.id);

    let mut seen = HashSet::with_capacity(world_entities.len());

    for entity_state in world_entities {
        seen.insert(entity_state.id);
        if let Some(entity) = index.by_id.get(&entity_state.id).copied() {
            if let Ok((mut transform, mut sprite)) = visuals.get_mut(entity) {
                apply_visual_state(&entity_state, &mut transform, &mut sprite);
                continue;
            }
            index.by_id.remove(&entity_state.id);
        }

        let spawned = spawn_visual_entity(&mut commands, &entity_state);
        index.by_id.insert(entity_state.id, spawned);
    }

    let stale: Vec<(u64, Entity)> = index
        .by_id
        .iter()
        .filter_map(|(id, entity)| (!seen.contains(id)).then_some((*id, *entity)))
        .collect();
    for (id, entity) in stale {
        commands.entity(entity).despawn();
        index.by_id.remove(&id);
    }
}

fn spawn_visual_entity(commands: &mut Commands, state: &EntityState) -> Entity {
    let mut sprite = Sprite::default();
    let mut transform = Transform::default();
    apply_visual_state(state, &mut transform, &mut sprite);
    commands.spawn((sprite, transform, WorldVisualEntity)).id()
}

fn apply_visual_state(state: &EntityState, transform: &mut Transform, sprite: &mut Sprite) {
    let z = match state.kind {
        EntityKind::Player => 0.22,
        EntityKind::Pellet => 0.15,
    };
    let render_pos = sim_to_render_pos(state.pos, z);
    transform.translation = render_pos;
    sprite.custom_size = Some(Vec2::splat((state.radius * 2.0).max(2.0)));
    sprite.color = Color::srgb(state.color[0], state.color[1], state.color[2]);
}

fn sim_to_render_pos(pos: [f32; 2], z: f32) -> Vec3 {
    Vec3::new(pos[0] - WORLD_WIDTH * 0.5, pos[1] - WORLD_HEIGHT * 0.5, z)
}

fn update_network_panel(
    runtime: Res<NetRuntime>,
    sim_tick: Res<SimTick>,
    mut panel_text: Query<&mut Text, With<NetPanelText>>,
) {
    let Ok(mut panel_text) = panel_text.single_mut() else {
        return;
    };

    let Some((clients, total_up, total_down, total_rtt, total_loss, sampled, per_client_lines)) =
        runtime.net.with_server_and_transport(|server, transport| {
            let mut clients = server.clients_id();
            clients.sort_unstable();

            let mut total_up = 0.0_f64;
            let mut total_down = 0.0_f64;
            let mut total_rtt = 0.0_f64;
            let mut total_loss = 0.0_f64;
            let mut sampled = 0_u64;
            let mut per_client_lines = Vec::new();

            for client_id in &clients {
                if let Ok(info) = server.network_info(*client_id) {
                    total_up += info.bytes_sent_per_second;
                    total_down += info.bytes_received_per_second;
                    total_rtt += info.rtt;
                    total_loss += info.packet_loss;
                    sampled += 1;

                    let transport_label = if transport.udp().client_addr(*client_id).is_some() {
                        "udp"
                    } else if transport.webrtc().client_addr(*client_id).is_some() {
                        "webrtc"
                    } else {
                        "unknown"
                    };

                    per_client_lines.push(format!(
                        "id={} transport={} rtt={:.1}ms loss={:.2}% up={:.1}KiB/s down={:.1}KiB/s",
                        client_id,
                        transport_label,
                        info.rtt * 1000.0,
                        info.packet_loss * 100.0,
                        info.bytes_sent_per_second / 1024.0,
                        info.bytes_received_per_second / 1024.0
                    ));
                }
            }

            (
                clients,
                total_up,
                total_down,
                total_rtt,
                total_loss,
                sampled,
                per_client_lines,
            )
        })
    else {
        panel_text.0 = "network panel unavailable: server/transport lock poisoned".to_owned();
        return;
    };

    let avg_rtt_ms = if sampled == 0 {
        0.0
    } else {
        (total_rtt / sampled as f64) * 1000.0
    };
    let avg_loss_pct = if sampled == 0 {
        0.0
    } else {
        (total_loss / sampled as f64) * 100.0
    };

    let mut lines = vec![
        "Server Diagnostics".to_owned(),
        format!("sim_tick: {}", sim_tick.tick),
        format!("connected_clients: {}", clients.len()),
        format!("avg_rtt: {:.1} ms", avg_rtt_ms),
        format!("avg_packet_loss: {:.2}%", avg_loss_pct),
        format!("total_up: {:.1} KiB/s", total_up / 1024.0),
        format!("total_down: {:.1} KiB/s", total_down / 1024.0),
        "camera: WASD/Arrows, Shift boost".to_owned(),
        String::new(),
        "Per-client network state:".to_owned(),
    ];

    if per_client_lines.is_empty() {
        lines.push("  (no connected clients)".to_owned());
    } else {
        lines.extend(per_client_lines.into_iter().take(12));
    }

    panel_text.0 = lines.join("\n");
}

fn radius_from_mass(mass: f32) -> f32 {
    mass.sqrt() * 2.2
}

fn normalize(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= f32::EPSILON {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

fn distance_sq(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    dx * dx + dy * dy
}

fn clamp_axis(value: f32, max: f32) -> f32 {
    value.clamp(0.0, max)
}

fn spawn_point_from_id(id: u64) -> [f32; 2] {
    let center_x = WORLD_WIDTH * 0.5;
    let center_y = WORLD_HEIGHT * 0.5;

    let angle_seed = ((id.wrapping_mul(1103515245).wrapping_add(12345)) % 10_000) as f32 / 10_000.0;
    let radius_seed = ((id.wrapping_mul(214013).wrapping_add(2531011)) % 10_000) as f32 / 10_000.0;

    let angle = angle_seed * TAU;
    let radius = 120.0 + radius_seed * 260.0;

    [
        center_x + angle.cos() * radius,
        center_y + angle.sin() * radius,
    ]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if h < 1.0 {
        (c, x, 0.0)
    } else if h < 2.0 {
        (x, c, 0.0)
    } else if h < 3.0 {
        (0.0, c, x)
    } else if h < 4.0 {
        (0.0, x, c)
    } else if h < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

fn rand_u32(rng_state: &mut u64) -> u32 {
    *rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*rng_state >> 32) as u32
}

fn rand_range(rng_state: &mut u64, min: f32, max: f32) -> f32 {
    let t = rand_u32(rng_state) as f32 / u32::MAX as f32;
    min + (max - min) * t
}
