//! A symmetric authoritative duel built from OpenStrike's existing movement,
//! collision, player, weapon, and hitscan primitives.
//!
//! This module intentionally lives outside the pinned upstream submodule. It
//! proves the smallest useful integration boundary without maintaining a
//! long-lived fork of OpenStrike's renderer or JavaScript product layer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glam::Vec3;
use openstrike_core::{Player, Weapon};
use pocket3d_bsp::{Hull, MapCollision, MapData, SpawnPoint};
use serde::Serialize;

use crate::{
    TICK_HZ, devmap,
    protocol::{InputFrame, MatchPhase, PlayerSlot, PlayerSnapshot, WorldSnapshot},
    sim::{MatchEvent, MatchSimulation},
};

const DT: f32 = 1.0 / TICK_HZ as f32;
const PLAYER_HALF: Vec3 = Vec3::new(16.0, 36.0, 16.0);
const PLAYER_PUSH_DISTANCE: f32 = 34.0;

struct Duelist {
    player: Player,
    weapon: Weapon,
    last_input_sequence: u32,
}

impl Duelist {
    fn new(spawn: SpawnPoint) -> Self {
        Self {
            player: Player::spawn(spawn.pos, spawn.yaw),
            weapon: Weapon::default(),
            last_input_sequence: 0,
        }
    }

    fn snapshot(&self, slot: PlayerSlot) -> PlayerSnapshot {
        let player = &self.player;
        PlayerSnapshot {
            slot,
            position: player.state.pos.to_array(),
            velocity: player.state.vel.to_array(),
            yaw: player.yaw,
            pitch: player.pitch,
            on_ground: player.state.on_ground,
            health: player.health.max(0) as u16,
            alive: player.alive,
            ammo: self.weapon.ammo,
            reserve: self.weapon.reserve,
            reloading: self.weapon.reloading(),
            last_input_sequence: self.last_input_sequence,
        }
    }
}

/// The authoritative simulation used by the eventual OpenStrike match server.
/// Rendering remains a client concern; the server only needs BSP collision.
pub struct OpenStrikeDuelSim {
    collision: MapCollision,
    dev_arena: bool,
    phase: MatchPhase,
    players: [Duelist; 2],
}

impl OpenStrikeDuelSim {
    pub fn new(collision: MapCollision, spawns: [SpawnPoint; 2]) -> Self {
        Self {
            collision,
            dev_arena: false,
            phase: MatchPhase::Waiting,
            players: [Duelist::new(spawns[0]), Duelist::new(spawns[1])],
        }
    }

    pub fn load(bsp_path: &Path, wad_dirs: &[PathBuf]) -> Result<(Self, MapData)> {
        let mut map = pocket3d_bsp::load_map(bsp_path, wad_dirs)
            .with_context(|| format!("loading OpenStrike map {}", bsp_path.display()))?;
        let a = map
            .ct_spawns
            .first()
            .copied()
            .or_else(|| map.t_spawns.first().copied())
            .context("map has no player spawn")?;
        let b = map
            .t_spawns
            .first()
            .copied()
            .or_else(|| map.ct_spawns.get(1).copied())
            .context("map needs a second player spawn")?;
        let collision = std::mem::replace(&mut map.collision, empty_collision());
        Ok((Self::new(collision, [a, b]), map))
    }

    /// Asset-free neon arena used by CI and first-run desktop smoke tests.
    /// Layout, movement collision, and hitscan occlusion come from
    /// [`devmap`]; gravity is disabled because the floor is perfectly flat.
    pub fn dev_arena() -> Self {
        let mut simulation = Self::new(empty_collision(), devmap::spawns());
        simulation.dev_arena = true;
        for player in &mut simulation.players {
            player.player.params.gravity = 0.0;
            player.player.state.on_ground = true;
        }
        simulation
    }

    pub fn player(&self, slot: PlayerSlot) -> PlayerSnapshot {
        self.players[slot.index()].snapshot(slot)
    }

    fn apply_input(&mut self, slot: PlayerSlot, input: InputFrame) {
        let duelist = &mut self.players[slot.index()];
        if !duelist.player.alive || input.sequence < duelist.last_input_sequence {
            return;
        }
        let input = input.sanitized();
        duelist.last_input_sequence = input.sequence;
        duelist.player.prev_pos = duelist.player.state.pos;
        duelist.player.yaw = input.yaw;
        duelist.player.pitch = input.pitch;
        duelist.weapon.tick(DT);
        if input.reload {
            duelist.weapon.trigger_reload();
        }

        let mut wish = duelist.player.forward_flat() * input.move_y;
        wish += duelist.player.right() * input.move_x;
        if self.dev_arena {
            devmap::flat_step(&mut duelist.player, wish, input.walk, DT);
            return;
        }
        let movement = pocket3d_bsp::collide::MoveInput {
            wish_dir: wish,
            speed: if input.walk {
                openstrike_core::sim::WALK_SPEED_SCALE
            } else {
                1.0
            },
            jump: input.jump,
        };
        pocket3d_bsp::collide::step_character(
            &self.collision,
            pocket3d_bsp::collide::HullKind::Stand,
            &mut duelist.player.state,
            &duelist.player.params,
            &movement,
            DT,
        );
    }

    fn fire(&mut self, attacker: PlayerSlot, input: InputFrame) -> Vec<MatchEvent> {
        if !input.fire || !self.players[attacker.index()].player.alive {
            return Vec::new();
        }
        let victim = attacker.opponent();
        if !self.players[victim.index()].player.alive
            || !self.players[attacker.index()].weapon.fire()
        {
            return Vec::new();
        }

        let shooter = &self.players[attacker.index()].player;
        let eye = shooter.eye();
        let direction = shooter.view_dir();
        let max_distance = if self.dev_arena {
            devmap::trace_distance(eye, direction, openstrike_core::weapon::RANGE)
        } else {
            let world_hit = self.collision.trace(
                Hull::Point,
                eye,
                eye + direction * openstrike_core::weapon::RANGE,
            );
            world_hit.fraction * openstrike_core::weapon::RANGE
        };
        let target_center = self.players[victim.index()].player.state.pos;
        let Some(hit_distance) = openstrike_core::sim::ray_aabb(
            eye,
            direction,
            target_center - PLAYER_HALF,
            target_center + PLAYER_HALF,
        ) else {
            return Vec::new();
        };
        if hit_distance >= max_distance {
            return Vec::new();
        }

        let hit_point = eye + direction * hit_distance;
        let damage_body = self.players[attacker.index()].weapon.cfg.damage_body;
        let damage_head = self.players[attacker.index()].weapon.cfg.damage_head;
        let target = &mut self.players[victim.index()];
        let headshot = hit_point.y > target.player.state.pos.y + 22.0;
        let damage = if headshot { damage_head } else { damage_body };
        target.player.health = (target.player.health - damage).max(0);
        let amount = damage.max(0) as u16;
        let mut events = vec![MatchEvent::Damage {
            attacker,
            victim,
            amount,
            victim_health: target.player.health as u16,
        }];
        if target.player.health == 0 {
            target.player.alive = false;
            self.phase = MatchPhase::Ended { winner: attacker };
            events.push(MatchEvent::Death {
                killer: attacker,
                victim,
            });
        }
        events
    }

    fn separate_players(&mut self) {
        let delta = self.players[PlayerSlot::A.index()].player.state.pos
            - self.players[PlayerSlot::B.index()].player.state.pos;
        let horizontal = Vec3::new(delta.x, 0.0, delta.z);
        let distance = horizontal.length();
        if distance <= 0.001
            || distance >= PLAYER_PUSH_DISTANCE
            || delta.y.abs() >= PLAYER_HALF.y * 2.0
        {
            return;
        }
        let push = horizontal / distance * (PLAYER_PUSH_DISTANCE - distance) * 0.5;
        self.players[PlayerSlot::A.index()].player.state.pos += push;
        self.players[PlayerSlot::B.index()].player.state.pos -= push;
    }
}

impl MatchSimulation for OpenStrikeDuelSim {
    fn set_phase(&mut self, phase: MatchPhase) {
        self.phase = phase;
    }

    fn phase(&self) -> MatchPhase {
        self.phase
    }

    fn tick(&mut self, inputs: [InputFrame; 2]) -> Vec<MatchEvent> {
        if self.phase != MatchPhase::Live {
            return Vec::new();
        }
        for slot in PlayerSlot::ALL {
            self.apply_input(slot, inputs[slot.index()]);
        }
        self.separate_players();
        let mut events = self.fire(PlayerSlot::A, inputs[PlayerSlot::A.index()]);
        if self.phase == MatchPhase::Live {
            events.extend(self.fire(PlayerSlot::B, inputs[PlayerSlot::B.index()]));
        }
        events
    }

    fn world_snapshot(
        &self,
        match_id: u128,
        server_tick: u64,
        latest_settlement_sequence: u64,
    ) -> WorldSnapshot {
        WorldSnapshot {
            match_id,
            server_tick,
            phase: self.phase,
            players: [self.player(PlayerSlot::A), self.player(PlayerSlot::B)],
            latest_settlement_sequence,
        }
    }

    fn state_hash(&self) -> [u8; 32] {
        #[derive(Serialize)]
        struct HashableState {
            phase: MatchPhase,
            players: [PlayerSnapshot; 2],
        }
        *blake3::hash(
            &postcard::to_stdvec(&HashableState {
                phase: self.phase,
                players: [self.player(PlayerSlot::A), self.player(PlayerSlot::B)],
            })
            .expect("OpenStrike duel state is serializable"),
        )
        .as_bytes()
    }
}

pub fn empty_collision() -> MapCollision {
    use pocket3d_bsp::trace::ModelHulls;
    MapCollision::from_parts(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![ModelHulls {
            headnodes: [-1; 4],
            origin: Vec3::ZERO,
        }],
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn duel() -> OpenStrikeDuelSim {
        OpenStrikeDuelSim::new(
            empty_collision(),
            [
                SpawnPoint {
                    pos: Vec3::new(0.0, 0.0, 100.0),
                    yaw: 0.0,
                },
                SpawnPoint {
                    pos: Vec3::new(0.0, 0.0, -100.0),
                    yaw: std::f32::consts::PI,
                },
            ],
        )
    }

    #[test]
    fn uses_openstrike_weapon_damage_and_cooldown() {
        let mut duel = duel();
        duel.set_phase(MatchPhase::Live);
        let first = duel.tick([
            InputFrame {
                sequence: 1,
                yaw: 0.0,
                pitch: -0.05,
                fire: true,
                ..Default::default()
            },
            InputFrame::default(),
        ]);
        assert!(matches!(
            first.as_slice(),
            [MatchEvent::Damage {
                attacker: PlayerSlot::A,
                victim: PlayerSlot::B,
                amount: 34,
                victim_health: 66,
            }]
        ));

        let second = duel.tick([
            InputFrame {
                sequence: 2,
                yaw: 0.0,
                pitch: -0.05,
                fire: true,
                ..Default::default()
            },
            InputFrame::default(),
        ]);
        assert!(
            second.is_empty(),
            "weapon cooldown must reject the next tick"
        );
    }

    #[test]
    fn snapshot_exposes_independent_weapon_state() {
        let mut duel = duel();
        duel.set_phase(MatchPhase::Live);
        duel.tick([
            InputFrame {
                sequence: 1,
                yaw: 0.0,
                fire: true,
                ..Default::default()
            },
            InputFrame::default(),
        ]);
        assert_eq!(duel.player(PlayerSlot::A).ammo, 29);
        assert_eq!(duel.player(PlayerSlot::B).ammo, 30);
    }

    #[test]
    fn simultaneous_body_shots_are_symmetric() {
        let mut duel = OpenStrikeDuelSim::dev_arena();
        // The dev arena's monolith blocks the spawn-to-spawn sightline by
        // design, so place both duelists in the open mid-lane for this test.
        duel.players[PlayerSlot::A.index()].player.state.pos = Vec3::new(120.0, 0.0, 100.0);
        duel.players[PlayerSlot::B.index()].player.state.pos = Vec3::new(120.0, 0.0, -100.0);
        duel.set_phase(MatchPhase::Live);
        let events = duel.tick([
            InputFrame {
                sequence: 1,
                yaw: 0.0,
                pitch: -0.05,
                fire: true,
                ..Default::default()
            },
            InputFrame {
                sequence: 1,
                yaw: std::f32::consts::PI,
                pitch: -0.05,
                fire: true,
                ..Default::default()
            },
        ]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, MatchEvent::Damage { .. }))
                .count(),
            2
        );
        assert_eq!(duel.player(PlayerSlot::A).health, 66);
        assert_eq!(duel.player(PlayerSlot::B).health, 66);
    }
}
