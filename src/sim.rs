use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

use crate::protocol::{InputFrame, MatchPhase, PlayerSlot, PlayerSnapshot, WorldSnapshot};

const DT: f32 = 1.0 / crate::TICK_HZ as f32;
const ARENA_HALF_SIZE: f32 = 18.0;
const MOVE_SPEED: f32 = 7.0;
const SHOT_RANGE: f32 = 32.0;
const SHOT_COOLDOWN_TICKS: u64 = crate::TICK_HZ / 5;
const AIM_DOT_THRESHOLD: f32 = 0.978_147_6; // cos(12 degrees)
const SHOT_DAMAGE: u16 = 25;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MatchEvent {
    Damage {
        attacker: PlayerSlot,
        victim: PlayerSlot,
        amount: u16,
        victim_health: u16,
    },
    Death {
        killer: PlayerSlot,
        victim: PlayerSlot,
    },
}

pub trait MatchSimulation {
    fn set_phase(&mut self, phase: MatchPhase);
    fn phase(&self) -> MatchPhase;
    fn tick(&mut self, inputs: [InputFrame; 2]) -> Vec<MatchEvent>;
    fn world_snapshot(
        &self,
        match_id: u128,
        server_tick: u64,
        latest_settlement_sequence: u64,
    ) -> WorldSnapshot;
    fn state_hash(&self) -> [u8; 32];
}

#[derive(Clone, Copy, Debug, Serialize)]
struct HarnessPlayer {
    position: [f32; 3],
    velocity: [f32; 3],
    yaw: f32,
    health: u16,
    alive: bool,
    last_input_sequence: u32,
    last_shot_tick: Option<u64>,
}

impl HarnessPlayer {
    fn snapshot(self, slot: PlayerSlot) -> PlayerSnapshot {
        PlayerSnapshot {
            slot,
            position: self.position,
            velocity: self.velocity,
            yaw: self.yaw,
            pitch: 0.0,
            on_ground: true,
            health: self.health,
            alive: self.alive,
            ammo: 30,
            reserve: 90,
            reloading: false,
            last_input_sequence: self.last_input_sequence,
        }
    }
}

/// A deterministic two-player simulation used to validate networking and
/// payment behavior before copyrighted map data is available. It deliberately
/// implements the same small boundary the OpenStrike adapter will implement.
pub struct HarnessSim {
    tick: u64,
    phase: MatchPhase,
    players: [HarnessPlayer; 2],
}

impl Default for HarnessSim {
    fn default() -> Self {
        Self {
            tick: 0,
            phase: MatchPhase::Waiting,
            players: [
                HarnessPlayer {
                    position: [-8.0, 0.0, 0.0],
                    velocity: [0.0; 3],
                    yaw: 0.0,
                    health: 100,
                    alive: true,
                    last_input_sequence: 0,
                    last_shot_tick: None,
                },
                HarnessPlayer {
                    position: [8.0, 0.0, 0.0],
                    velocity: [0.0; 3],
                    yaw: PI,
                    health: 100,
                    alive: true,
                    last_input_sequence: 0,
                    last_shot_tick: None,
                },
            ],
        }
    }
}

impl HarnessSim {
    pub fn player(&self, slot: PlayerSlot) -> PlayerSnapshot {
        self.players[slot.index()].snapshot(slot)
    }

    fn update_player(&mut self, slot: PlayerSlot, input: InputFrame) {
        let player = &mut self.players[slot.index()];
        if !player.alive || input.sequence < player.last_input_sequence {
            return;
        }
        let input = input.sanitized();
        player.last_input_sequence = input.sequence;
        player.yaw = input.yaw;
        player.velocity = [input.move_x * MOVE_SPEED, 0.0, input.move_y * MOVE_SPEED];
        player.position[0] =
            (player.position[0] + player.velocity[0] * DT).clamp(-ARENA_HALF_SIZE, ARENA_HALF_SIZE);
        player.position[2] =
            (player.position[2] + player.velocity[2] * DT).clamp(-ARENA_HALF_SIZE, ARENA_HALF_SIZE);
    }

    fn can_fire(&self, slot: PlayerSlot) -> bool {
        self.players[slot.index()]
            .last_shot_tick
            .is_none_or(|last| self.tick.saturating_sub(last) >= SHOT_COOLDOWN_TICKS)
    }

    fn shot_hits(&self, attacker: PlayerSlot) -> bool {
        let victim = attacker.opponent();
        let from = self.players[attacker.index()].position;
        let to = self.players[victim.index()].position;
        let dx = to[0] - from[0];
        let dz = to[2] - from[2];
        let distance = (dx * dx + dz * dz).sqrt();
        if distance <= f32::EPSILON || distance > SHOT_RANGE {
            return false;
        }
        let yaw = self.players[attacker.index()].yaw;
        let forward = [yaw.cos(), yaw.sin()];
        let direction = [dx / distance, dz / distance];
        forward[0] * direction[0] + forward[1] * direction[1] >= AIM_DOT_THRESHOLD
    }

    fn fire(&mut self, attacker: PlayerSlot, input: InputFrame) -> Vec<MatchEvent> {
        if !input.fire
            || !self.players[attacker.index()].alive
            || !self.players[attacker.opponent().index()].alive
            || !self.can_fire(attacker)
        {
            return Vec::new();
        }
        self.players[attacker.index()].last_shot_tick = Some(self.tick);
        if !self.shot_hits(attacker) {
            return Vec::new();
        }

        let victim = attacker.opponent();
        let target = &mut self.players[victim.index()];
        target.health = target.health.saturating_sub(SHOT_DAMAGE);
        let mut events = vec![MatchEvent::Damage {
            attacker,
            victim,
            amount: SHOT_DAMAGE,
            victim_health: target.health,
        }];
        if target.health == 0 {
            target.alive = false;
            self.phase = MatchPhase::Ended { winner: attacker };
            events.push(MatchEvent::Death {
                killer: attacker,
                victim,
            });
        }
        events
    }
}

impl MatchSimulation for HarnessSim {
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
        self.tick += 1;
        for slot in PlayerSlot::ALL {
            self.update_player(slot, inputs[slot.index()]);
        }
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
        *blake3::hash(&postcard::to_stdvec(&self.players).expect("sim state is serializable"))
            .as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aimed_shots_damage_and_kill() {
        let mut sim = HarnessSim::default();
        sim.set_phase(MatchPhase::Live);
        let mut a = InputFrame {
            fire: true,
            yaw: 0.0,
            ..Default::default()
        };

        let mut damage_events = 0;
        for sequence in 1..=4 {
            a.sequence = sequence;
            let events = sim.tick([a, InputFrame::default()]);
            damage_events += events
                .iter()
                .filter(|event| matches!(event, MatchEvent::Damage { .. }))
                .count();
            for _ in 0..SHOT_COOLDOWN_TICKS {
                sim.tick([InputFrame::default(), InputFrame::default()]);
            }
        }

        assert_eq!(damage_events, 4);
        assert_eq!(sim.player(PlayerSlot::B).health, 0);
        assert_eq!(
            sim.phase(),
            MatchPhase::Ended {
                winner: PlayerSlot::A
            }
        );
    }

    #[test]
    fn missed_shot_does_no_damage() {
        let mut sim = HarnessSim::default();
        sim.set_phase(MatchPhase::Live);
        sim.tick([
            InputFrame {
                fire: true,
                yaw: PI / 2.0,
                ..Default::default()
            },
            InputFrame::default(),
        ]);
        assert_eq!(sim.player(PlayerSlot::B).health, 100);
    }
}
