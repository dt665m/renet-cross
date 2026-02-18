use std::{
    collections::{HashMap, HashSet},
    f32::consts::TAU,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const TICK_RATE_HZ: u32 = 30;
pub const FIXED_DT_SECONDS: f32 = 1.0 / TICK_RATE_HZ as f32;
pub const WORLD_WIDTH: f32 = 3000.0;
pub const WORLD_HEIGHT: f32 = 3000.0;
pub const TARGET_PELLET_COUNT: usize = 256;
pub const BASE_PLAYER_MASS: f32 = 12.0;
pub const PELLET_MASS: f32 = 1.0;
pub const PLAYER_CONSUME_RATIO: f32 = 1.15;
pub const RESPAWN_TICKS: u32 = TICK_RATE_HZ * 2;
pub const BASE_PLAYER_SPEED: f32 = 420.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntityKind {
    Player,
    Pellet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityState {
    pub id: u64,
    pub kind: EntityKind,
    pub pos: [f32; 2],
    pub vel: [f32; 2],
    pub mass: f32,
    pub radius: f32,
    pub color: [f32; 3],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorldState {
    pub tick: u32,
    pub entities: Vec<EntityState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct ClientInput {
    pub seq: u32,
    pub move_dir: [f32; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorldEvent {
    PelletConsumed { player_id: u64, pellet_id: u64 },
    PlayerConsumed { consumer_id: u64, consumed_id: u64 },
    PlayerRespawned { player_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JoinSnapshot {
    pub you: u64,
    pub tick: u32,
    pub your_last_input_seq: Option<u32>,
    pub world: WorldState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorldDelta {
    pub tick: u32,
    pub your_last_input_seq: Option<u32>,
    pub upserts: Vec<EntityState>,
    pub removed: Vec<u64>,
    pub events: Vec<WorldEvent>,
}

#[derive(Debug, Clone)]
struct RespawnEntry {
    client_id: u64,
    at_tick: u32,
}

#[derive(Debug)]
pub struct Simulation {
    tick: u32,
    next_entity_id: u64,
    rng_state: u64,
    players: HashMap<u64, EntityState>,
    player_colors: HashMap<u64, [f32; 3]>,
    pellets: HashMap<u64, EntityState>,
    inputs: HashMap<u64, ClientInput>,
    respawn_queue: Vec<RespawnEntry>,
    snapshot_cache: HashMap<u64, EntityState>,
}

impl Default for Simulation {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Simulation {
    pub fn new(seed: u64) -> Self {
        Self {
            tick: 0,
            next_entity_id: 1_000_000,
            rng_state: seed.max(1),
            players: HashMap::new(),
            player_colors: HashMap::new(),
            pellets: HashMap::new(),
            inputs: HashMap::new(),
            respawn_queue: Vec::new(),
            snapshot_cache: HashMap::new(),
        }
    }

    pub fn tick(&self) -> u32 {
        self.tick
    }

    pub fn ensure_player(&mut self, client_id: u64) {
        if self.players.contains_key(&client_id) {
            return;
        }

        let mut pos = spawn_point_from_id(client_id);
        pos[0] = clamp_axis(pos[0], WORLD_WIDTH);
        pos[1] = clamp_axis(pos[1], WORLD_HEIGHT);
        let color = if let Some(color) = self.player_colors.get(&client_id).copied() {
            color
        } else {
            let color = self.generate_player_color();
            self.player_colors.insert(client_id, color);
            color
        };

        self.players.insert(
            client_id,
            EntityState {
                id: client_id,
                kind: EntityKind::Player,
                pos,
                vel: [0.0, 0.0],
                mass: BASE_PLAYER_MASS,
                radius: radius_from_mass(BASE_PLAYER_MASS),
                color,
            },
        );
    }

    pub fn remove_player(&mut self, client_id: u64) {
        self.players.remove(&client_id);
        self.player_colors.remove(&client_id);
        self.inputs.remove(&client_id);
        self.respawn_queue
            .retain(|entry| entry.client_id != client_id);
    }

    pub fn apply_input(&mut self, client_id: u64, input: ClientInput) {
        self.inputs.insert(client_id, input);
    }

    pub fn join_snapshot(&mut self, client_id: u64) -> JoinSnapshot {
        self.ensure_player(client_id);
        JoinSnapshot {
            you: client_id,
            tick: self.tick,
            your_last_input_seq: self.inputs.get(&client_id).map(|input| input.seq),
            world: self.world_state(),
        }
    }

    pub fn world_state(&self) -> WorldState {
        let mut entities: Vec<EntityState> = self.players.values().cloned().collect();
        entities.extend(self.pellets.values().cloned());
        entities.sort_unstable_by_key(|entity| entity.id);

        WorldState {
            tick: self.tick,
            entities,
        }
    }

    pub fn step(&mut self) -> WorldDelta {
        self.tick += 1;

        let mut events = Vec::new();

        self.handle_respawns(&mut events);
        self.update_player_movement();
        self.consume_pellets(&mut events);
        self.consume_players(&mut events);
        self.maintain_pellet_budget();

        self.build_delta(events)
    }

    fn handle_respawns(&mut self, events: &mut Vec<WorldEvent>) {
        let mut pending = Vec::new();
        let mut due = Vec::new();
        for entry in std::mem::take(&mut self.respawn_queue) {
            if entry.at_tick <= self.tick {
                due.push(entry.client_id);
            } else {
                pending.push(entry);
            }
        }
        for client_id in due {
            self.ensure_player(client_id);
            events.push(WorldEvent::PlayerRespawned {
                player_id: client_id,
            });
        }
        self.respawn_queue = pending;
    }

    fn update_player_movement(&mut self) {
        for (client_id, player) in &mut self.players {
            let dir = self
                .inputs
                .get(client_id)
                .map(|input| normalize(input.move_dir))
                .unwrap_or([0.0, 0.0]);

            let speed = BASE_PLAYER_SPEED / (1.0 + player.mass * 0.015);
            player.vel = [dir[0] * speed, dir[1] * speed];
            player.pos[0] = clamp_axis(
                player.pos[0] + player.vel[0] * FIXED_DT_SECONDS,
                WORLD_WIDTH,
            );
            player.pos[1] = clamp_axis(
                player.pos[1] + player.vel[1] * FIXED_DT_SECONDS,
                WORLD_HEIGHT,
            );
        }
    }

    fn consume_pellets(&mut self, events: &mut Vec<WorldEvent>) {
        let player_ids: Vec<u64> = self.players.keys().copied().collect();

        for player_id in player_ids {
            let Some(player) = self.players.get(&player_id).cloned() else {
                continue;
            };

            let mut consumed = Vec::new();
            for (pellet_id, pellet) in &self.pellets {
                let dist_sq = distance_sq(player.pos, pellet.pos);
                let max_dist = player.radius + pellet.radius;
                if dist_sq <= max_dist * max_dist {
                    consumed.push(*pellet_id);
                }
            }

            if consumed.is_empty() {
                continue;
            }

            let Some(player_mut) = self.players.get_mut(&player_id) else {
                continue;
            };

            for pellet_id in consumed {
                if self.pellets.remove(&pellet_id).is_some() {
                    player_mut.mass += PELLET_MASS;
                    player_mut.radius = radius_from_mass(player_mut.mass);
                    events.push(WorldEvent::PelletConsumed {
                        player_id,
                        pellet_id,
                    });
                }
            }
        }
    }

    fn consume_players(&mut self, events: &mut Vec<WorldEvent>) {
        let ids: Vec<u64> = self.players.keys().copied().collect();
        let mut dead: HashSet<u64> = HashSet::new();

        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let a_id = ids[i];
                let b_id = ids[j];
                if dead.contains(&a_id) || dead.contains(&b_id) {
                    continue;
                }

                let Some(a) = self.players.get(&a_id).cloned() else {
                    continue;
                };
                let Some(b) = self.players.get(&b_id).cloned() else {
                    continue;
                };

                let dist_sq = distance_sq(a.pos, b.pos);
                let max_dist = (a.radius + b.radius) * 0.8;
                if dist_sq > max_dist * max_dist {
                    continue;
                }

                let (consumer_id, consumed_id, mass_gain) =
                    if a.mass >= b.mass * PLAYER_CONSUME_RATIO {
                        (a_id, b_id, b.mass * 0.8)
                    } else if b.mass >= a.mass * PLAYER_CONSUME_RATIO {
                        (b_id, a_id, a.mass * 0.8)
                    } else {
                        continue;
                    };

                if let Some(consumer) = self.players.get_mut(&consumer_id) {
                    consumer.mass += mass_gain;
                    consumer.radius = radius_from_mass(consumer.mass);
                }

                self.players.remove(&consumed_id);
                self.inputs.remove(&consumed_id);
                self.respawn_queue.push(RespawnEntry {
                    client_id: consumed_id,
                    at_tick: self.tick + RESPAWN_TICKS,
                });
                dead.insert(consumed_id);

                events.push(WorldEvent::PlayerConsumed {
                    consumer_id,
                    consumed_id,
                });
            }
        }
    }

    fn maintain_pellet_budget(&mut self) {
        while self.pellets.len() < TARGET_PELLET_COUNT {
            let id = self.next_entity_id;
            self.next_entity_id += 1;

            let x = self.rand_range(0.0, WORLD_WIDTH);
            let y = self.rand_range(0.0, WORLD_HEIGHT);

            self.pellets.insert(
                id,
                EntityState {
                    id,
                    kind: EntityKind::Pellet,
                    pos: [x, y],
                    vel: [0.0, 0.0],
                    mass: PELLET_MASS,
                    radius: radius_from_mass(PELLET_MASS),
                    color: [0.96, 0.76, 0.31],
                },
            );
        }
    }

    fn build_delta(&mut self, events: Vec<WorldEvent>) -> WorldDelta {
        let mut current: HashMap<u64, EntityState> = HashMap::new();
        for entity in self.players.values() {
            current.insert(entity.id, entity.clone());
        }
        for entity in self.pellets.values() {
            current.insert(entity.id, entity.clone());
        }

        let mut upserts = Vec::new();
        for (id, entity) in &current {
            let changed = self.snapshot_cache.get(id) != Some(entity);
            if changed {
                upserts.push(entity.clone());
            }
        }
        upserts.sort_unstable_by_key(|entity| entity.id);

        let mut removed = Vec::new();
        for old_id in self.snapshot_cache.keys() {
            if !current.contains_key(old_id) {
                removed.push(*old_id);
            }
        }
        removed.sort_unstable();

        self.snapshot_cache = current;

        WorldDelta {
            tick: self.tick,
            your_last_input_seq: None,
            upserts,
            removed,
            events,
        }
    }

    fn rand_u32(&mut self) -> u32 {
        // Numerical Recipes LCG
        self.rng_state = self
            .rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        (self.rng_state >> 32) as u32
    }

    fn rand_range(&mut self, min: f32, max: f32) -> f32 {
        let t = self.rand_u32() as f32 / u32::MAX as f32;
        min + (max - min) * t
    }

    fn generate_player_color(&mut self) -> [f32; 3] {
        let hue = self.rand_range(0.0, 360.0);
        hsv_to_rgb(hue, 0.72, 0.88)
    }
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    bincode::serialize(value).expect("serialize")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, bincode::Error> {
    bincode::deserialize(bytes)
}

pub fn is_newer_input_seq(candidate: u32, latest: u32) -> bool {
    let delta = candidate.wrapping_sub(latest);
    delta != 0 && delta < (u32::MAX / 2)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_integration_changes_position() {
        let mut sim = Simulation::new(7);
        sim.ensure_player(1);
        let initial_x = sim.players.get(&1).expect("player").pos[0];

        sim.apply_input(
            1,
            ClientInput {
                seq: 1,
                move_dir: [1.0, 0.0],
            },
        );
        sim.step();

        let next_x = sim.players.get(&1).expect("player").pos[0];
        assert!(next_x > initial_x);
    }

    #[test]
    fn pellet_budget_is_maintained() {
        let mut sim = Simulation::new(3);
        let _ = sim.step();
        assert!(sim.pellets.len() >= TARGET_PELLET_COUNT);
    }

    #[test]
    fn larger_player_consumes_smaller_player() {
        let mut sim = Simulation::new(9);
        sim.ensure_player(10);
        sim.ensure_player(20);

        {
            let p = sim.players.get_mut(&10).expect("p1");
            p.mass = 30.0;
            p.radius = radius_from_mass(p.mass);
            p.pos = [500.0, 500.0];
        }
        {
            let p = sim.players.get_mut(&20).expect("p2");
            p.mass = 5.0;
            p.radius = radius_from_mass(p.mass);
            p.pos = [500.0, 500.0];
        }

        let delta = sim.step();
        assert!(delta.events.iter().any(|event| matches!(
            event,
            WorldEvent::PlayerConsumed {
                consumer_id: 10,
                consumed_id: 20
            }
        )));
    }

    #[test]
    fn delta_contains_upserts_and_removals() {
        let mut sim = Simulation::new(1);
        sim.ensure_player(42);
        let d1 = sim.step();
        assert!(d1.upserts.iter().any(|entity| entity.id == 42));

        sim.remove_player(42);
        let d2 = sim.step();
        assert!(d2.removed.contains(&42));
    }
}
