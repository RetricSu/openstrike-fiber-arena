//! Generates `assets/models/duelist.glb`: the original low-poly neon duelist
//! that replaced the third-party Mixamo soldier.
//!
//! Everything is procedural — skeleton, mesh, palette texture, and the
//! Idle/Walk/Run/Death clips are all built in code and written as a single
//! self-contained GLB, so the game ships zero third-party character art.
//! Re-run after editing:
//!
//! ```sh
//! cargo run --features openstrike --bin build-duelist
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use glam::{Mat4, Quat, Vec3};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Skeleton
// ---------------------------------------------------------------------------

struct Bone {
    name: &'static str,
    parent: Option<usize>,
    /// Rest-pose translation, local to the parent bone.
    translation: Vec3,
}

const HIPS: usize = 0;
const SPINE: usize = 1;
const CHEST: usize = 2;
const HEAD: usize = 3;
const UPPER_ARM_L: usize = 4;
const LOWER_ARM_L: usize = 5;
const HAND_L: usize = 6;
const UPPER_ARM_R: usize = 7;
const LOWER_ARM_R: usize = 8;
const HAND_R: usize = 9;
const UPPER_LEG_L: usize = 10;
const LOWER_LEG_L: usize = 11;
const FOOT_L: usize = 12;
const UPPER_LEG_R: usize = 13;
const LOWER_LEG_R: usize = 14;
const FOOT_R: usize = 15;

fn bones() -> Vec<Bone> {
    vec![
        Bone {
            name: "Hips",
            parent: None,
            translation: Vec3::new(0.0, 36.0, 0.0),
        },
        Bone {
            name: "Spine",
            parent: Some(HIPS),
            translation: Vec3::new(0.0, 4.0, 0.0),
        },
        Bone {
            name: "Chest",
            parent: Some(SPINE),
            translation: Vec3::new(0.0, 6.0, 0.0),
        },
        Bone {
            name: "Head",
            parent: Some(CHEST),
            translation: Vec3::new(0.0, 8.0, 0.0),
        },
        Bone {
            name: "UpperArmL",
            parent: Some(CHEST),
            translation: Vec3::new(-5.5, 5.0, 0.0),
        },
        Bone {
            name: "LowerArmL",
            parent: Some(UPPER_ARM_L),
            translation: Vec3::new(0.0, -10.0, 0.0),
        },
        Bone {
            name: "HandL",
            parent: Some(LOWER_ARM_L),
            translation: Vec3::new(0.0, -9.0, 0.0),
        },
        Bone {
            name: "UpperArmR",
            parent: Some(CHEST),
            translation: Vec3::new(5.5, 5.0, 0.0),
        },
        Bone {
            name: "LowerArmR",
            parent: Some(UPPER_ARM_R),
            translation: Vec3::new(0.0, -10.0, 0.0),
        },
        Bone {
            name: "HandR",
            parent: Some(LOWER_ARM_R),
            translation: Vec3::new(0.0, -9.0, 0.0),
        },
        Bone {
            name: "UpperLegL",
            parent: Some(HIPS),
            translation: Vec3::new(-2.8, -2.0, 0.0),
        },
        Bone {
            name: "LowerLegL",
            parent: Some(UPPER_LEG_L),
            translation: Vec3::new(0.0, -14.0, 0.0),
        },
        Bone {
            name: "FootL",
            parent: Some(LOWER_LEG_L),
            translation: Vec3::new(0.0, -14.0, 0.0),
        },
        Bone {
            name: "UpperLegR",
            parent: Some(HIPS),
            translation: Vec3::new(2.8, -2.0, 0.0),
        },
        Bone {
            name: "LowerLegR",
            parent: Some(UPPER_LEG_R),
            translation: Vec3::new(0.0, -14.0, 0.0),
        },
        Bone {
            name: "FootR",
            parent: Some(LOWER_LEG_R),
            translation: Vec3::new(0.0, -14.0, 0.0),
        },
    ]
}

fn rest_globals(bones: &[Bone]) -> Vec<Mat4> {
    let mut globals = vec![Mat4::IDENTITY; bones.len()];
    for (i, bone) in bones.iter().enumerate() {
        let local = Mat4::from_translation(bone.translation);
        globals[i] = match bone.parent {
            Some(parent) => globals[parent] * local,
            None => local,
        };
    }
    globals
}

// ---------------------------------------------------------------------------
// Palette (one texel per material slot, sampled at texel centers)
// ---------------------------------------------------------------------------

const PALETTE: [[u8; 4]; 8] = [
    [52, 62, 92, 255],    // 0 armor dark
    [88, 102, 140, 255],  // 1 armor mid
    [26, 30, 42, 255],    // 2 undersuit
    [240, 250, 255, 255], // 3 neon accent (white: picks up the team tint)
    [34, 38, 52, 255],    // 4 gloves / boots
    [240, 250, 255, 255], // 5 spare
    [240, 250, 255, 255], // 6 spare
    [240, 250, 255, 255], // 7 spare
];

const ARMOR_DARK: usize = 0;
const ARMOR_MID: usize = 1;
const UNDERSUIT: usize = 2;
const ACCENT: usize = 3;
const GLOVE: usize = 4;

// ---------------------------------------------------------------------------
// Mesh: rigid boxes, one bone per part
// ---------------------------------------------------------------------------

struct Part {
    bone: usize,
    min: Vec3,
    max: Vec3,
    color: usize,
}

fn parts() -> Vec<Part> {
    let p = |bone: usize, min: Vec3, max: Vec3, color: usize| Part {
        bone,
        min,
        max,
        color,
    };
    vec![
        // Torso.
        p(
            HIPS,
            Vec3::new(-4.5, 33.0, -2.75),
            Vec3::new(4.5, 39.5, 2.75),
            ARMOR_MID,
        ),
        p(
            CHEST,
            Vec3::new(-5.5, 39.5, -3.0),
            Vec3::new(5.5, 53.5, 3.0),
            ARMOR_DARK,
        ),
        p(
            CHEST,
            Vec3::new(-4.2, 47.0, -3.3),
            Vec3::new(4.2, 49.0, -2.9),
            ACCENT,
        ),
        // Head + visor + antenna fin.
        p(
            HEAD,
            Vec3::new(-3.0, 54.0, -2.75),
            Vec3::new(3.0, 63.0, 3.0),
            ARMOR_MID,
        ),
        p(
            HEAD,
            Vec3::new(-2.6, 58.5, -3.05),
            Vec3::new(2.6, 60.5, -2.65),
            ACCENT,
        ),
        p(
            HEAD,
            Vec3::new(-0.75, 63.0, -1.5),
            Vec3::new(0.75, 65.5, 1.5),
            ACCENT,
        ),
        // Left arm.
        p(
            UPPER_ARM_L,
            Vec3::new(-8.5, 49.5, -2.5),
            Vec3::new(-4.5, 53.0, 2.5),
            ARMOR_MID,
        ),
        p(
            UPPER_ARM_L,
            Vec3::new(-7.0, 41.0, -2.0),
            Vec3::new(-4.0, 51.5, 2.0),
            ARMOR_DARK,
        ),
        p(
            LOWER_ARM_L,
            Vec3::new(-6.75, 32.0, -1.75),
            Vec3::new(-4.25, 41.5, 1.75),
            UNDERSUIT,
        ),
        p(
            LOWER_ARM_L,
            Vec3::new(-6.9, 36.5, -1.9),
            Vec3::new(-4.1, 38.0, 1.9),
            ACCENT,
        ),
        p(
            HAND_L,
            Vec3::new(-6.5, 28.5, -1.5),
            Vec3::new(-4.5, 32.5, 1.5),
            GLOVE,
        ),
        // Right arm.
        p(
            UPPER_ARM_R,
            Vec3::new(4.5, 49.5, -2.5),
            Vec3::new(8.5, 53.0, 2.5),
            ARMOR_MID,
        ),
        p(
            UPPER_ARM_R,
            Vec3::new(4.0, 41.0, -2.0),
            Vec3::new(7.0, 51.5, 2.0),
            ARMOR_DARK,
        ),
        p(
            LOWER_ARM_R,
            Vec3::new(4.25, 32.0, -1.75),
            Vec3::new(6.75, 41.5, 1.75),
            UNDERSUIT,
        ),
        p(
            LOWER_ARM_R,
            Vec3::new(4.1, 36.5, -1.9),
            Vec3::new(6.9, 38.0, 1.9),
            ACCENT,
        ),
        p(
            HAND_R,
            Vec3::new(4.5, 28.5, -1.5),
            Vec3::new(6.5, 32.5, 1.5),
            GLOVE,
        ),
        // Left leg.
        p(
            UPPER_LEG_L,
            Vec3::new(-4.75, 19.0, -2.5),
            Vec3::new(-0.85, 34.5, 2.5),
            ARMOR_DARK,
        ),
        p(
            LOWER_LEG_L,
            Vec3::new(-4.5, 5.5, -2.0),
            Vec3::new(-1.1, 19.5, 2.0),
            UNDERSUIT,
        ),
        p(
            FOOT_L,
            Vec3::new(-4.5, 0.0, -5.0),
            Vec3::new(-1.1, 6.0, 2.5),
            GLOVE,
        ),
        // Right leg.
        p(
            UPPER_LEG_R,
            Vec3::new(0.85, 19.0, -2.5),
            Vec3::new(4.75, 34.5, 2.5),
            ARMOR_DARK,
        ),
        p(
            LOWER_LEG_R,
            Vec3::new(1.1, 5.5, -2.0),
            Vec3::new(4.5, 19.5, 2.0),
            UNDERSUIT,
        ),
        p(
            FOOT_R,
            Vec3::new(1.1, 0.0, -5.0),
            Vec3::new(4.5, 6.0, 2.5),
            GLOVE,
        ),
    ]
}

struct Mesh {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    joints: Vec<[u8; 4]>,
    weights: Vec<[f32; 4]>,
    indices: Vec<u16>,
}

fn build_mesh() -> Mesh {
    let mut mesh = Mesh {
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        joints: Vec::new(),
        weights: Vec::new(),
        indices: Vec::new(),
    };
    for part in parts() {
        let uv = [(part.color as f32 + 0.5) / PALETTE.len() as f32, 0.5];
        let c = |x: f32, y: f32, z: f32| {
            Vec3::new(
                if x > 0.0 { part.max.x } else { part.min.x },
                if y > 0.0 { part.max.y } else { part.min.y },
                if z > 0.0 { part.max.z } else { part.min.z },
            )
        };
        let faces: [(Vec3, [Vec3; 4]); 6] = [
            (
                Vec3::X,
                [
                    c(1.0, -1.0, 1.0),
                    c(1.0, -1.0, -1.0),
                    c(1.0, 1.0, -1.0),
                    c(1.0, 1.0, 1.0),
                ],
            ),
            (
                -Vec3::X,
                [
                    c(-1.0, -1.0, -1.0),
                    c(-1.0, -1.0, 1.0),
                    c(-1.0, 1.0, 1.0),
                    c(-1.0, 1.0, -1.0),
                ],
            ),
            (
                Vec3::Y,
                [
                    c(-1.0, 1.0, 1.0),
                    c(1.0, 1.0, 1.0),
                    c(1.0, 1.0, -1.0),
                    c(-1.0, 1.0, -1.0),
                ],
            ),
            (
                -Vec3::Y,
                [
                    c(-1.0, -1.0, -1.0),
                    c(1.0, -1.0, -1.0),
                    c(1.0, -1.0, 1.0),
                    c(-1.0, -1.0, 1.0),
                ],
            ),
            (
                Vec3::Z,
                [
                    c(-1.0, -1.0, 1.0),
                    c(1.0, -1.0, 1.0),
                    c(1.0, 1.0, 1.0),
                    c(-1.0, 1.0, 1.0),
                ],
            ),
            (
                -Vec3::Z,
                [
                    c(1.0, -1.0, -1.0),
                    c(-1.0, -1.0, -1.0),
                    c(-1.0, 1.0, -1.0),
                    c(1.0, 1.0, -1.0),
                ],
            ),
        ];
        for (normal, quad) in faces {
            let base = mesh.positions.len() as u16;
            for position in quad {
                mesh.positions.push(position.to_array());
                mesh.normals.push(normal.to_array());
                mesh.uvs.push(uv);
                mesh.joints.push([part.bone as u8, 0, 0, 0]);
                mesh.weights.push([1.0, 0.0, 0.0, 0.0]);
            }
            mesh.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// Animation clips
// ---------------------------------------------------------------------------

fn qx(deg: f32) -> Quat {
    Quat::from_rotation_x(deg.to_radians())
}
#[allow(dead_code)]
fn qy(deg: f32) -> Quat {
    Quat::from_rotation_y(deg.to_radians())
}
fn qz(deg: f32) -> Quat {
    Quat::from_rotation_z(deg.to_radians())
}

/// Arms wrapped around the (separately attached) rifle: right hand on the
/// grip by the chest, left hand forward on the handguard.
fn rifle_pose() -> [(usize, Quat); 6] {
    [
        (UPPER_ARM_R, qx(-8.0) * qz(-10.0)),
        (LOWER_ARM_R, qx(150.0)),
        (HAND_R, qx(15.0)),
        (UPPER_ARM_L, qx(55.0) * qz(25.0)),
        (LOWER_ARM_L, qx(95.0) * qz(-35.0)),
        (HAND_L, qx(15.0)),
    ]
}

struct Clip {
    name: &'static str,
    /// Uniform key times shared by every channel of this clip.
    times: Vec<f32>,
    /// (bone, rotation per key) channels; bones absent here hold the rest pose.
    rotations: Vec<(usize, Vec<Quat>)>,
    /// Hips translation per key (root bone gets root motion).
    hips_translation: Vec<Vec3>,
}

fn idle_clip() -> Clip {
    let times = vec![0.0, 0.6, 1.2, 1.8, 2.4];
    let keys = times.len();
    let mut rotations = vec![
        (HIPS, vec![Quat::IDENTITY; keys]),
        (SPINE, vec![Quat::IDENTITY; keys]),
    ];
    // Breathing: slow chest sway.
    rotations.push((CHEST, vec![qx(0.0), qx(1.6), qx(0.0), qx(-1.2), qx(0.0)]));
    rotations.push((HEAD, vec![qx(0.0), qx(-1.0), qx(0.0), qx(1.0), qx(0.0)]));
    for (bone, rot) in rifle_pose() {
        rotations.push((bone, vec![rot; keys]));
    }
    for leg in [
        UPPER_LEG_L,
        LOWER_LEG_L,
        FOOT_L,
        UPPER_LEG_R,
        LOWER_LEG_R,
        FOOT_R,
    ] {
        rotations.push((leg, vec![Quat::IDENTITY; keys]));
    }
    Clip {
        name: "Idle",
        times,
        rotations,
        hips_translation: vec![
            Vec3::new(0.0, 36.0, 0.0),
            Vec3::new(0.0, 35.6, 0.0),
            Vec3::new(0.0, 36.0, 0.0),
            Vec3::new(0.0, 36.3, 0.0),
            Vec3::new(0.0, 36.0, 0.0),
        ],
    }
}

fn gait_clip(name: &'static str, period: f32, swing: f32, knee: f32, lean: f32, bob: f32) -> Clip {
    // Eight keys per cycle so the sine reads smoothly at 64 Hz.
    let keys_n = 8;
    let times: Vec<f32> = (0..=keys_n)
        .map(|i| i as f32 * period / keys_n as f32)
        .collect();
    let keys = times.len();
    let phase = |i: usize| i as f32 / keys_n as f32 * std::f32::consts::TAU;
    let mut rotations = vec![
        (HIPS, vec![Quat::IDENTITY; keys]),
        (SPINE, vec![Quat::IDENTITY; keys]),
        (CHEST, (0..keys).map(|_| qx(lean)).collect()),
        (HEAD, (0..keys).map(|_| qx(-lean * 0.7)).collect()),
    ];
    for (bone, rot) in rifle_pose() {
        rotations.push((bone, vec![rot; keys]));
    }
    let leg_l: Vec<Quat> = (0..keys).map(|i| qx(swing * phase(i).sin())).collect();
    let leg_r: Vec<Quat> = (0..keys).map(|i| qx(-swing * phase(i).sin())).collect();
    rotations.push((UPPER_LEG_L, leg_l));
    rotations.push((UPPER_LEG_R, leg_r));
    // Knees flex backward (heel rises behind) as the leg swings through;
    // feet stay roughly level.
    rotations.push((
        LOWER_LEG_L,
        (0..keys)
            .map(|i| qx(-knee * 0.5 * (1.0 - (phase(i) + 0.6).cos())))
            .collect(),
    ));
    rotations.push((
        LOWER_LEG_R,
        (0..keys)
            .map(|i| qx(-knee * 0.5 * (1.0 - (phase(i) + std::f32::consts::PI + 0.6).cos())))
            .collect(),
    ));
    rotations.push((
        FOOT_L,
        (0..keys)
            .map(|i| qx(-swing * 0.35 * phase(i).sin()))
            .collect(),
    ));
    rotations.push((
        FOOT_R,
        (0..keys)
            .map(|i| qx(swing * 0.35 * phase(i).sin()))
            .collect(),
    ));
    Clip {
        name,
        times,
        rotations,
        hips_translation: (0..keys)
            .map(|i| Vec3::new(0.0, 36.0 + bob * (phase(i) * 2.0).cos(), 0.0))
            .collect(),
    }
}

fn death_clip() -> Clip {
    let times = vec![0.0, 0.22, 0.5, 0.78, 0.95, 1.15];
    let rifle = rifle_pose();
    let arm = |bone: usize| -> (usize, Vec<Quat>) {
        let hold = rifle
            .iter()
            .find(|(b, _)| *b == bone)
            .map(|(_, r)| *r)
            .unwrap_or(Quat::IDENTITY);
        let fling = match bone {
            UPPER_ARM_L => qx(-95.0) * qz(-25.0),
            UPPER_ARM_R => qx(-95.0) * qz(25.0),
            LOWER_ARM_L | LOWER_ARM_R => qx(-25.0),
            HAND_L | HAND_R => Quat::IDENTITY,
            _ => hold,
        };
        let rest = match bone {
            UPPER_ARM_L => qx(-75.0) * qz(-18.0),
            UPPER_ARM_R => qx(-75.0) * qz(18.0),
            LOWER_ARM_L | LOWER_ARM_R => qx(-12.0),
            HAND_L | HAND_R => Quat::IDENTITY,
            _ => hold,
        };
        (bone, vec![hold, fling, fling, fling, rest, rest])
    };
    Clip {
        name: "Death",
        times: times.clone(),
        rotations: vec![
            (
                HIPS,
                vec![qx(0.0), qx(16.0), qx(58.0), qx(88.0), qx(82.0), qx(88.0)],
            ),
            (SPINE, vec![Quat::IDENTITY; 6]),
            (
                CHEST,
                vec![qx(0.0), qx(4.0), qx(6.0), qx(6.0), qx(6.0), qx(6.0)],
            ),
            (
                HEAD,
                vec![
                    qx(0.0),
                    qx(-14.0),
                    qx(-20.0),
                    qx(-24.0),
                    qx(-24.0),
                    qx(-24.0),
                ],
            ),
            arm(UPPER_ARM_L),
            arm(LOWER_ARM_L),
            arm(HAND_L),
            arm(UPPER_ARM_R),
            arm(LOWER_ARM_R),
            arm(HAND_R),
            (
                UPPER_LEG_L,
                vec![qx(0.0), qx(-18.0), qx(-8.0), qx(0.0), qx(0.0), qx(0.0)],
            ),
            (
                LOWER_LEG_L,
                vec![qx(0.0), qx(-34.0), qx(-14.0), qx(-2.0), qx(-2.0), qx(-2.0)],
            ),
            (FOOT_L, vec![Quat::IDENTITY; 6]),
            (
                UPPER_LEG_R,
                vec![qx(0.0), qx(-22.0), qx(-10.0), qx(0.0), qx(0.0), qx(0.0)],
            ),
            (
                LOWER_LEG_R,
                vec![qx(0.0), qx(-38.0), qx(-16.0), qx(-3.0), qx(-3.0), qx(-3.0)],
            ),
            (FOOT_R, vec![Quat::IDENTITY; 6]),
        ],
        hips_translation: vec![
            Vec3::new(0.0, 36.0, 0.0),
            Vec3::new(0.0, 29.0, 1.5),
            Vec3::new(0.0, 13.0, 5.0),
            Vec3::new(0.0, 5.5, 8.0),
            Vec3::new(0.0, 6.5, 8.0),
            Vec3::new(0.0, 5.5, 8.0),
        ],
    }
}

// ---------------------------------------------------------------------------
// GLB assembly
// ---------------------------------------------------------------------------

const FLOAT: u32 = 5126;
const UBYTE: u32 = 5121;
const USHORT: u32 = 5123;

struct Glb {
    bin: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Glb {
    fn new() -> Self {
        Self {
            bin: Vec::new(),
            views: Vec::new(),
            accessors: Vec::new(),
        }
    }

    fn push_view(&mut self, bytes: &[u8]) -> usize {
        while !self.bin.len().is_multiple_of(4) {
            self.bin.push(0);
        }
        let offset = self.bin.len();
        self.bin.extend_from_slice(bytes);
        self.views.push(json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": bytes.len(),
        }));
        self.views.len() - 1
    }

    fn push_accessor(
        &mut self,
        component: u32,
        kind: &str,
        count: usize,
        bytes: &[u8],
        min_max: Option<(Value, Value)>,
    ) -> usize {
        let view = self.push_view(bytes);
        let mut accessor = json!({
            "bufferView": view,
            "componentType": component,
            "count": count,
            "type": kind,
        });
        if let Some((min, max)) = min_max {
            accessor["min"] = min;
            accessor["max"] = max;
        }
        self.accessors.push(accessor);
        self.accessors.len() - 1
    }

    fn push_f32(
        &mut self,
        kind: &str,
        data: &[f32],
        components: usize,
        min_max: Option<(Value, Value)>,
    ) -> usize {
        let bytes: Vec<u8> = data.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.push_accessor(FLOAT, kind, data.len() / components, &bytes, min_max)
    }
}

fn build_glb() -> Result<Vec<u8>> {
    let bones = bones();
    let mesh = build_mesh();
    let clips = vec![
        idle_clip(),
        gait_clip("Walk", 0.9, 26.0, 34.0, 4.0, 0.7),
        gait_clip("Run", 0.55, 44.0, 58.0, 9.0, 1.4),
        death_clip(),
    ];

    let mut glb = Glb::new();

    // Geometry accessors.
    let positions: Vec<f32> = mesh.positions.into_iter().flatten().collect();
    let (min, max) =
        positions
            .as_chunks::<3>()
            .0
            .iter()
            .fold(([f32::MAX; 3], [f32::MIN; 3]), |(mn, mx), p| {
                (
                    [mn[0].min(p[0]), mn[1].min(p[1]), mn[2].min(p[2])],
                    [mx[0].max(p[0]), mx[1].max(p[1]), mx[2].max(p[2])],
                )
            });
    let position_acc = glb.push_f32("VEC3", &positions, 3, Some((json!(min), json!(max))));
    let normals: Vec<f32> = mesh.normals.into_iter().flatten().collect();
    let normal_acc = glb.push_f32("VEC3", &normals, 3, None);
    let uvs: Vec<f32> = mesh.uvs.into_iter().flatten().collect();
    let uv_acc = glb.push_f32("VEC2", &uvs, 2, None);
    let joint_bytes: Vec<u8> = mesh.joints.into_iter().flatten().collect();
    let joint_acc = glb.push_accessor(UBYTE, "VEC4", joint_bytes.len() / 4, &joint_bytes, None);
    let weights: Vec<f32> = mesh.weights.into_iter().flatten().collect();
    let weight_acc = glb.push_f32("VEC4", &weights, 4, None);
    let index_bytes: Vec<u8> = mesh.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
    let index_acc = glb.push_accessor(USHORT, "SCALAR", mesh.indices.len(), &index_bytes, None);

    // Inverse bind matrices.
    let globals = rest_globals(&bones);
    let ibm: Vec<f32> = globals
        .iter()
        .flat_map(|g| g.inverse().to_cols_array())
        .collect();
    let ibm_acc = glb.push_f32("MAT4", &ibm, 16, None);

    // Palette texture PNG.
    let mut rgba = Vec::with_capacity(PALETTE.len() * 4);
    for texel in PALETTE {
        rgba.extend_from_slice(&texel);
    }
    let png = encode_png(PALETTE.len() as u32, 1, &rgba);
    let image_view = glb.push_view(&png);

    // Animation accessors.
    let mut animations = Vec::new();
    for clip in &clips {
        let time_acc = glb.push_f32(
            "SCALAR",
            &clip.times,
            1,
            Some((
                json!([clip.times[0]]),
                json!([clip.times[clip.times.len() - 1]]),
            )),
        );
        let mut samplers = Vec::new();
        let mut channels = Vec::new();
        for (bone, rotations) in &clip.rotations {
            let values: Vec<f32> = rotations
                .iter()
                .flat_map(|q| q.normalize().to_array())
                .collect();
            let output = glb.push_f32("VEC4", &values, 4, None);
            samplers.push(json!({
                "input": time_acc,
                "output": output,
                "interpolation": "LINEAR",
            }));
            channels.push(json!({
                "sampler": samplers.len() - 1,
                "target": { "node": bone, "path": "rotation" },
            }));
        }
        let translations: Vec<f32> = clip
            .hips_translation
            .iter()
            .flat_map(|t| t.to_array())
            .collect();
        let output = glb.push_f32("VEC3", &translations, 3, None);
        samplers.push(json!({
            "input": time_acc,
            "output": output,
            "interpolation": "LINEAR",
        }));
        channels.push(json!({
            "sampler": samplers.len() - 1,
            "target": { "node": HIPS, "path": "translation" },
        }));
        animations.push(json!({
            "name": clip.name,
            "samplers": samplers,
            "channels": channels,
        }));
    }

    // Nodes: 16 bones + the skinned mesh node.
    let mut nodes = Vec::new();
    for (i, bone) in bones.iter().enumerate() {
        let children: Vec<usize> = bones
            .iter()
            .enumerate()
            .filter(|(_, b)| b.parent == Some(i))
            .map(|(j, _)| j)
            .collect();
        let mut node = json!({
            "name": bone.name,
            "translation": bone.translation.to_array(),
        });
        if !children.is_empty() {
            node["children"] = json!(children);
        }
        nodes.push(node);
    }
    let mesh_node = nodes.len();
    nodes.push(json!({ "name": "Duelist", "mesh": 0, "skin": 0 }));

    let document = json!({
        "asset": { "version": "2.0", "generator": "openstrike-fiber-arena build-duelist" },
        "scene": 0,
        "scenes": [{ "nodes": [HIPS, mesh_node] }],
        "nodes": nodes,
        "skins": [{
            "joints": (0..bones.len()).collect::<Vec<_>>(),
            "inverseBindMatrices": ibm_acc,
            "skeleton": HIPS,
        }],
        "meshes": [{
            "name": "duelist",
            "primitives": [{
                "attributes": {
                    "POSITION": position_acc,
                    "NORMAL": normal_acc,
                    "TEXCOORD_0": uv_acc,
                    "JOINTS_0": joint_acc,
                    "WEIGHTS_0": weight_acc,
                },
                "indices": index_acc,
                "material": 0,
            }],
        }],
        "materials": [{
            "name": "duelist-palette",
            "pbrMetallicRoughness": {
                "baseColorTexture": { "index": 0 },
                "metallicFactor": 0.0,
                "roughnessFactor": 0.9,
            },
        }],
        "textures": [{ "sampler": 0, "source": 0 }],
        "images": [{
            "name": "palette",
            "mimeType": "image/png",
            "bufferView": image_view,
        }],
        "samplers": [{ "magFilter": 9728, "minFilter": 9728, "wrapS": 10497, "wrapT": 10497 }],
        "animations": animations,
        "accessors": glb.accessors,
        "bufferViews": glb.views,
        "buffers": [{ "byteLength": glb.bin.len() }],
    });

    // GLB container: header + JSON chunk + BIN chunk.
    let mut json_bytes = serde_json::to_vec(&document)?;
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }
    let mut bin = glb.bin;
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Minimal PNG encoder (stored DEFLATE blocks; no compression, no deps)
// ---------------------------------------------------------------------------

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for row in rgba.chunks((width * 4) as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }

    let mut zlib = vec![0x78, 0x01];
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        zlib.push(u8::from(last));
        zlib.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &zlib);
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc_data = Vec::with_capacity(4 + data.len());
    crc_data.extend_from_slice(kind);
    crc_data.extend_from_slice(data);
    png.extend_from_slice(&crc32(&crc_data).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn main() -> Result<()> {
    let out_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/models/duelist.glb");
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let glb = build_glb()?;
    std::fs::write(&out_path, &glb).with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "wrote {} ({} bytes, {} bones, 4 clips)",
        out_path.display(),
        glb.len(),
        bones().len()
    );
    Ok(())
}
