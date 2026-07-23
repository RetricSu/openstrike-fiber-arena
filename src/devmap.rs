//! The built-in neon duel arena: one shared source of truth for layout,
//! movement collision, and hitscan occlusion.
//!
//! Both the authoritative server ([`crate::openstrike`]) and the desktop
//! client (movement prediction and tracer endpoints) consume this module, so
//! cover blocks players and bullets identically on both sides. Rendering
//! consumes the same obstacle list to draw the arena, which keeps the visible
//! geometry and the collision geometry in lockstep by construction.
//!
//! The layout is symmetric under a 180° rotation around the origin, so
//! neither spawn has an advantage.

use glam::Vec3;
use openstrike_core::Player;
use pocket3d_bsp::SpawnPoint;

/// Player center hovers at y=0; feet rest on the floor at this height.
pub const FLOOR_Y: f32 = -36.0;
/// Visible floor half-extent; walls sit on this square.
pub const EXTENT: f32 = 512.0;
pub const WALL_HEIGHT: f32 = 132.0;
/// Movement clamp for the flat dev arena (keeps players off the walls).
pub const MOVE_CLAMP: f32 = 460.0;
/// Player collision half-extent on the horizontal plane.
pub const PLAYER_HALF_XZ: f32 = 16.0;

/// Waist-high cover: blocks body shots, head stays exposed.
pub const COVER_HEIGHT: f32 = 64.0;
/// Full-height blocker from floor to above head level.
pub const PYLON_HEIGHT: f32 = 118.0;

/// Player spawns, mirrored through the origin.
pub const SPAWN_DISTANCE: f32 = 260.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObstacleKind {
    /// The central full-height monolith that splits the arena.
    Monolith,
    /// Waist-high cover block.
    Cover,
    /// Full-height corner pylon.
    Pylon,
}

#[derive(Clone, Copy, Debug)]
pub struct Obstacle {
    pub min: Vec3,
    pub max: Vec3,
    pub kind: ObstacleKind,
}

const fn obstacle(x0: f32, z0: f32, x1: f32, z1: f32, height: f32, kind: ObstacleKind) -> Obstacle {
    Obstacle {
        min: Vec3::new(x0, FLOOR_Y, z0),
        max: Vec3::new(x1, FLOOR_Y + height, z1),
        kind,
    }
}

/// The arena obstacle set. Rotational symmetry: every entry at `(x, z)` has a
/// partner at `(-x, -z)`.
static OBSTACLES: &[Obstacle] = &[
    // Twin gate pillars at mid. The 36-unit slot between them is the classic
    // duel line: spawn-to-spawn shots thread the gate, but stepping off the
    // center line denies the angle.
    obstacle(
        -84.0,
        -40.0,
        -18.0,
        40.0,
        PYLON_HEIGHT,
        ObstacleKind::Monolith,
    ),
    obstacle(
        18.0,
        -40.0,
        84.0,
        40.0,
        PYLON_HEIGHT,
        ObstacleKind::Monolith,
    ),
    // Mid-lane cover pair, flanking the gate on opposite diagonals.
    obstacle(168.0, -24.0, 252.0, 60.0, COVER_HEIGHT, ObstacleKind::Cover),
    obstacle(
        -252.0,
        -60.0,
        -168.0,
        24.0,
        COVER_HEIGHT,
        ObstacleKind::Cover,
    ),
    // Flank crates guarding the side lanes.
    obstacle(
        -330.0,
        150.0,
        -246.0,
        234.0,
        COVER_HEIGHT,
        ObstacleKind::Cover,
    ),
    obstacle(
        246.0,
        -234.0,
        330.0,
        -150.0,
        COVER_HEIGHT,
        ObstacleKind::Cover,
    ),
    // Tall pylons anchoring the off-angles.
    obstacle(
        336.0,
        110.0,
        396.0,
        170.0,
        PYLON_HEIGHT,
        ObstacleKind::Pylon,
    ),
    obstacle(
        -396.0,
        -170.0,
        -336.0,
        -110.0,
        PYLON_HEIGHT,
        ObstacleKind::Pylon,
    ),
];

pub fn obstacles() -> &'static [Obstacle] {
    OBSTACLES
}

/// Two mirrored spawn points: slot A faces -z, slot B faces +z.
pub fn spawns() -> [SpawnPoint; 2] {
    [
        SpawnPoint {
            pos: Vec3::new(0.0, 0.0, SPAWN_DISTANCE),
            yaw: 0.0,
        },
        SpawnPoint {
            pos: Vec3::new(0.0, 0.0, -SPAWN_DISTANCE),
            yaw: std::f32::consts::PI,
        },
    ]
}

/// Flat dev-arena movement step shared by server and client prediction:
/// constant-velocity planar move, pushed out of obstacle AABBs, clamped to
/// the arena bounds.
pub fn flat_step(player: &mut Player, wish: Vec3, walk: bool, dt: f32) {
    let speed_scale = if walk {
        openstrike_core::sim::WALK_SPEED_SCALE
    } else {
        1.0
    };
    let velocity = wish.normalize_or_zero() * player.params.max_speed * speed_scale;
    player.state.vel = Vec3::new(velocity.x, 0.0, velocity.z);
    player.state.pos += player.state.vel * dt;
    resolve_collisions(&mut player.state.pos);
    player.state.pos.x = player.state.pos.x.clamp(-MOVE_CLAMP, MOVE_CLAMP);
    player.state.pos.z = player.state.pos.z.clamp(-MOVE_CLAMP, MOVE_CLAMP);
    player.state.pos.y = 0.0;
    player.state.on_ground = true;
}

/// Push the player center out of every obstacle (expanded by the player
/// radius) along the axis of least penetration. The arena is flat, so only
/// the horizontal axes need resolving.
fn resolve_collisions(pos: &mut Vec3) {
    for obstacle in obstacles() {
        let min_x = obstacle.min.x - PLAYER_HALF_XZ;
        let max_x = obstacle.max.x + PLAYER_HALF_XZ;
        let min_z = obstacle.min.z - PLAYER_HALF_XZ;
        let max_z = obstacle.max.z + PLAYER_HALF_XZ;
        if pos.x <= min_x || pos.x >= max_x || pos.z <= min_z || pos.z >= max_z {
            continue;
        }
        let exit_low_x = pos.x - min_x;
        let exit_high_x = max_x - pos.x;
        let exit_low_z = pos.z - min_z;
        let exit_high_z = max_z - pos.z;
        let min_exit = exit_low_x.min(exit_high_x).min(exit_low_z).min(exit_high_z);
        if min_exit == exit_low_x {
            pos.x = min_x;
        } else if min_exit == exit_high_x {
            pos.x = max_x;
        } else if min_exit == exit_low_z {
            pos.z = min_z;
        } else {
            pos.z = max_z;
        }
    }
}

/// Distance along `dir` to the nearest arena blocker (obstacle or boundary
/// wall), capped at `max`. Hitscan and tracer endpoints both use this so a
/// shot never visually passes through cover it was blocked by.
pub fn trace_distance(origin: Vec3, dir: Vec3, max: f32) -> f32 {
    let mut best = max;
    for obstacle in obstacles() {
        if let Some(t) = openstrike_core::sim::ray_aabb(origin, dir, obstacle.min, obstacle.max) {
            best = best.min(t);
        }
    }
    // Boundary walls as four outward-facing slabs.
    for (axis, wall) in [(0, EXTENT), (2, EXTENT)] {
        for sign in [-1.0, 1.0] {
            let d = dir[axis] * sign;
            if d <= f32::EPSILON {
                continue;
            }
            let t = (wall - origin[axis] * sign) / d;
            if t > 0.0 && t < best {
                let y = origin.y + dir.y * t;
                if (FLOOR_Y..FLOOR_Y + WALL_HEIGHT).contains(&y) {
                    best = t;
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_rotationally_symmetric() {
        // 180° rotation around the arena center, on the horizontal plane.
        for obstacle in obstacles() {
            let mirrored = obstacles().iter().any(|other| {
                other.kind == obstacle.kind
                    && (other.min.x + obstacle.max.x).abs() < 0.001
                    && (other.min.z + obstacle.max.z).abs() < 0.001
                    && (other.max.x + obstacle.min.x).abs() < 0.001
                    && (other.max.z + obstacle.min.z).abs() < 0.001
                    && (other.max.y - obstacle.max.y).abs() < 0.001
            });
            assert!(mirrored, "no mirrored partner for {obstacle:?}");
        }
    }

    #[test]
    fn spawn_duel_line_threads_the_gate() {
        // The spawns face each other down the mid slot: nothing blocks the
        // opening shot of a round.
        let [a, b] = spawns();
        let eye_a = a.pos + Vec3::Y * 28.0;
        let eye_b = b.pos + Vec3::Y * 28.0;
        let dir = (eye_b - eye_a).normalize();
        let distance = eye_a.distance(eye_b);
        assert_eq!(trace_distance(eye_a, dir, distance), distance);
    }

    #[test]
    fn stepping_off_the_mid_line_denies_the_angle() {
        // Wide of the gate, the pillar covers the peek across mid.
        let eye = Vec3::new(120.0, 28.0, 260.0);
        let target = Vec3::new(-120.0, 28.0, -260.0);
        let dir = (target - eye).normalize();
        assert!(trace_distance(eye, dir, 2000.0) < eye.distance(target));
    }

    #[test]
    fn movement_pushes_out_of_cover() {
        let mut player = Player::spawn(Vec3::new(150.0, 0.0, 18.0), 0.0);
        // Sprint straight at the mid-lane cover for a second.
        for _ in 0..64 {
            flat_step(&mut player, Vec3::X, false, 1.0 / 64.0);
        }
        assert!(player.state.pos.x <= 168.0 - PLAYER_HALF_XZ + 0.01);
    }

    #[test]
    fn shots_stop_at_cover() {
        // A level shot down the gate pillar's lane dies in the pillar.
        let origin = Vec3::new(50.0, 28.0, 200.0);
        let distance = trace_distance(origin, Vec3::new(0.0, 0.0, -1.0), 8192.0);
        assert!((distance - (200.0 - 40.0)).abs() < 0.01);
    }
}
