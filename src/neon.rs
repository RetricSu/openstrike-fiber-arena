//! The neon duel-arena art direction: procedural textures, the dev-arena
//! world geometry (built from [`crate::devmap`] so visuals match collision),
//! sky/lighting, and the neon rifle model data.
//!
//! Everything here is generated in code — no third-party art ships with the
//! game, and the `--dev-arena` path works out of the box.

use std::sync::Arc;

use pocket3d::{
    bsp::{
        Batch, DecodedTexture, MapData, MapGeometry, SpawnPoint, SunLight, SurfaceKind,
        WorldVertexData, lightmap::PAGE_SIZE, mesh::GeometryStats,
    },
    gpu::Gpu,
    model::{ModelAsset, ModelVertex},
    prelude::Vec3,
    renderer::Renderer,
    scene::{ModelLighting, Sky},
};

use crate::devmap::{self, ObstacleKind};

/// Team identity colors: slot A cyan, slot B orange. Applied as model tints
/// (the duelist's neon accents are authored white so they pick these up).
pub const TEAM_A: [f32; 4] = [0.45, 0.9, 1.0, 1.0];
pub const TEAM_B: [f32; 4] = [1.0, 0.72, 0.38, 1.0];

pub fn team_color(slot: crate::protocol::PlayerSlot) -> [f32; 4] {
    match slot {
        crate::protocol::PlayerSlot::A => TEAM_A,
        crate::protocol::PlayerSlot::B => TEAM_B,
    }
}

/// Spawn position by index (0 = cyan/A, 1 = orange/B).
pub fn devmap_spawn(index: usize) -> Vec3 {
    devmap::spawns()[index].pos
}

// ---------------------------------------------------------------------------
// Procedural textures (64x64 RGBA)
// ---------------------------------------------------------------------------

type Rgba = [u8; 4];

fn texture(name: &str, shade: impl Fn(u32, u32) -> Rgba) -> DecodedTexture {
    const SIZE: u32 = 64;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            rgba.extend_from_slice(&shade(x, y));
        }
    }
    DecodedTexture {
        name: name.into(),
        width: SIZE,
        height: SIZE,
        rgba,
        has_alpha: false,
    }
}

fn floor_texture() -> DecodedTexture {
    texture("neon-floor", |x, y| {
        // Glowing grid lines on a near-black panel floor.
        if x < 2 || y < 2 {
            return [34, 150, 185, 255];
        }
        if x < 5 && y < 5 {
            return [90, 225, 255, 255];
        }
        let panel = ((x / 32) + (y / 32)) % 2;
        let v = if panel == 0 { 12 } else { 15 };
        [v, v + 2, v + 10, 255]
    })
}

fn wall_texture() -> DecodedTexture {
    texture("neon-wall", |x, y| {
        // Dark paneling, magenta strip just above the floor, faint top rim.
        if (2..5).contains(&y) {
            return [150, 40, 140, 255];
        }
        if y >= 62 {
            return [70, 25, 70, 255];
        }
        if x % 16 == 0 {
            return [9, 10, 16, 255];
        }
        [14, 16, 26, 255]
    })
}

fn cover_texture() -> DecodedTexture {
    texture("neon-cover", |x, y| {
        // Cyan edge frame so cover reads at a glance.
        if x < 3 || y < 3 || x >= 61 || y >= 61 {
            return [52, 205, 240, 255];
        }
        if x < 5 || y < 5 || x >= 59 || y >= 59 {
            return [22, 40, 52, 255];
        }
        [17, 21, 33, 255]
    })
}

fn pylon_texture() -> DecodedTexture {
    texture("neon-pylon", |x, y| {
        // Vertical magenta energy stripes.
        if (6..10).contains(&x) || (54..58).contains(&x) {
            let pulse = if y % 16 < 8 { 170 } else { 120 };
            return [pulse, 35, (pulse as u16 * 9 / 10) as u8, 255];
        }
        if x < 1 || y < 1 || x >= 63 || y >= 63 {
            return [40, 18, 42, 255];
        }
        [12, 14, 23, 255]
    })
}

fn monolith_texture() -> DecodedTexture {
    texture("neon-monolith", |x, y| {
        // Central glowing rune column + scanlines.
        if (28..36).contains(&x) {
            let band = (y as i32 - 32).unsigned_abs() as u8;
            return [90 + band * 2, 110 + band, 255, 255];
        }
        if y % 8 == 0 {
            return [20, 24, 40, 255];
        }
        [10, 11, 18, 255]
    })
}

fn spawn_texture(name: &str, base: Rgba, ring: Rgba, core: Rgba) -> DecodedTexture {
    texture(name, |x, y| {
        let edge = x.min(y).min(63 - x).min(63 - y);
        if edge < 5 {
            return ring;
        }
        let dx = x as i32 - 32;
        let dy = y as i32 - 32;
        if dx * dx + dy * dy < 100 {
            return core;
        }
        base
    })
}

// ---------------------------------------------------------------------------
// World geometry
// ---------------------------------------------------------------------------

fn push_quad(
    vertices: &mut Vec<WorldVertexData>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
    texture: usize,
    points: [Vec3; 4],
    uv: [[f32; 2]; 4],
) {
    let base = vertices.len() as u32;
    let first_index = indices.len() as u32;
    for (pos, uv) in points.into_iter().zip(uv) {
        vertices.push(WorldVertexData {
            pos: pos.to_array(),
            uv,
            lm_uv: [uv[0].fract().abs(), uv[1].fract().abs()],
        });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    batches.push(Batch {
        texture,
        lm_page: 0,
        kind: SurfaceKind::Opaque,
        first_index,
        index_count: 6,
    });
}

#[allow(clippy::too_many_arguments)]
fn push_box(
    vertices: &mut Vec<WorldVertexData>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
    texture: usize,
    min: Vec3,
    max: Vec3,
) {
    let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    // Skip the bottom face: it sits inside the floor and is never visible.
    push_quad(
        vertices,
        indices,
        batches,
        texture,
        [
            Vec3::new(min.x, min.y, min.z),
            Vec3::new(max.x, min.y, min.z),
            Vec3::new(max.x, max.y, min.z),
            Vec3::new(min.x, max.y, min.z),
        ],
        uv,
    );
    push_quad(
        vertices,
        indices,
        batches,
        texture,
        [
            Vec3::new(max.x, min.y, max.z),
            Vec3::new(min.x, min.y, max.z),
            Vec3::new(min.x, max.y, max.z),
            Vec3::new(max.x, max.y, max.z),
        ],
        uv,
    );
    push_quad(
        vertices,
        indices,
        batches,
        texture,
        [
            Vec3::new(min.x, min.y, max.z),
            Vec3::new(min.x, min.y, min.z),
            Vec3::new(min.x, max.y, min.z),
            Vec3::new(min.x, max.y, max.z),
        ],
        uv,
    );
    push_quad(
        vertices,
        indices,
        batches,
        texture,
        [
            Vec3::new(max.x, min.y, min.z),
            Vec3::new(max.x, min.y, max.z),
            Vec3::new(max.x, max.y, max.z),
            Vec3::new(max.x, max.y, min.z),
        ],
        uv,
    );
    push_quad(
        vertices,
        indices,
        batches,
        texture,
        [
            Vec3::new(min.x, max.y, min.z),
            Vec3::new(max.x, max.y, min.z),
            Vec3::new(max.x, max.y, max.z),
            Vec3::new(min.x, max.y, max.z),
        ],
        uv,
    );
}

/// The asset-free neon arena. Geometry is generated from the same
/// [`devmap::obstacles`] the server simulates against, so a box you can see
/// is exactly the box that blocks you and your shots.
pub fn development_map() -> MapData {
    const FLOOR: usize = 0;
    const WALL: usize = 1;
    const COVER: usize = 2;
    const PYLON: usize = 3;
    const MONOLITH: usize = 4;
    const SPAWN_A: usize = 5;
    const SPAWN_B: usize = 6;

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut batches = Vec::new();

    let floor_y = devmap::FLOOR_Y;
    let extent = devmap::EXTENT;
    let wall_h = devmap::WALL_HEIGHT;

    let floor_uv = [[0.0, 0.0], [0.0, 20.0], [20.0, 20.0], [20.0, 0.0]];
    push_quad(
        &mut vertices,
        &mut indices,
        &mut batches,
        FLOOR,
        [
            Vec3::new(-extent, floor_y, -extent),
            Vec3::new(-extent, floor_y, extent),
            Vec3::new(extent, floor_y, extent),
            Vec3::new(extent, floor_y, -extent),
        ],
        floor_uv,
    );

    let wall_uv = [[0.0, 0.0], [16.0, 0.0], [16.0, 3.0], [0.0, 3.0]];
    for wall in [
        [
            Vec3::new(-extent, floor_y, -extent),
            Vec3::new(extent, floor_y, -extent),
            Vec3::new(extent, floor_y + wall_h, -extent),
            Vec3::new(-extent, floor_y + wall_h, -extent),
        ],
        [
            Vec3::new(extent, floor_y, extent),
            Vec3::new(-extent, floor_y, extent),
            Vec3::new(-extent, floor_y + wall_h, extent),
            Vec3::new(extent, floor_y + wall_h, extent),
        ],
        [
            Vec3::new(-extent, floor_y, extent),
            Vec3::new(-extent, floor_y, -extent),
            Vec3::new(-extent, floor_y + wall_h, -extent),
            Vec3::new(-extent, floor_y + wall_h, extent),
        ],
        [
            Vec3::new(extent, floor_y, -extent),
            Vec3::new(extent, floor_y, extent),
            Vec3::new(extent, floor_y + wall_h, extent),
            Vec3::new(extent, floor_y + wall_h, -extent),
        ],
    ] {
        push_quad(
            &mut vertices,
            &mut indices,
            &mut batches,
            WALL,
            wall,
            wall_uv,
        );
    }

    // Spawn pads, tinted per team.
    let pad = 74.0;
    let pad_uv = [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
    for (spawn, texture) in [
        (devmap::spawns()[0], SPAWN_A),
        (devmap::spawns()[1], SPAWN_B),
    ] {
        let z = spawn.pos.z;
        push_quad(
            &mut vertices,
            &mut indices,
            &mut batches,
            texture,
            [
                Vec3::new(-pad, floor_y + 0.6, z - pad),
                Vec3::new(-pad, floor_y + 0.6, z + pad),
                Vec3::new(pad, floor_y + 0.6, z + pad),
                Vec3::new(pad, floor_y + 0.6, z - pad),
            ],
            pad_uv,
        );
    }

    // Obstacles straight from the shared layout.
    for obstacle in devmap::obstacles() {
        let texture = match obstacle.kind {
            ObstacleKind::Cover => COVER,
            ObstacleKind::Pylon => PYLON,
            ObstacleKind::Monolith => MONOLITH,
        };
        push_box(
            &mut vertices,
            &mut indices,
            &mut batches,
            texture,
            obstacle.min,
            obstacle.max,
        );
    }

    let textures = vec![
        floor_texture(),
        wall_texture(),
        cover_texture(),
        pylon_texture(),
        monolith_texture(),
        spawn_texture(
            "spawn-a",
            [8, 18, 24, 255],
            [40, 210, 240, 255],
            [120, 240, 255, 255],
        ),
        spawn_texture(
            "spawn-b",
            [24, 12, 8, 255],
            [255, 150, 50, 255],
            [255, 195, 95, 255],
        ),
    ];

    // One uniform dim lightmap page: the arena is deliberately moody so the
    // neon edges carry the scene.
    let mut lightmap = vec![150; (PAGE_SIZE * PAGE_SIZE * 4) as usize];
    for alpha in lightmap.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }

    let quads = batches.len();
    MapData {
        name: "neon-arena".into(),
        geometry: MapGeometry {
            vertices,
            indices,
            batches,
            lightmap_pages: vec![lightmap],
            stats: GeometryStats {
                faces_drawn: quads,
                faces_skipped: 0,
                triangles: quads * 2,
            },
        },
        textures,
        entities: Vec::new(),
        collision: crate::openstrike::empty_collision(),
        ct_spawns: vec![SpawnPoint {
            pos: devmap::spawns()[0].pos,
            yaw: devmap::spawns()[0].yaw,
        }],
        t_spawns: vec![SpawnPoint {
            pos: devmap::spawns()[1].pos,
            yaw: devmap::spawns()[1].yaw,
        }],
        sun: Some(SunLight {
            dir: Vec3::new(0.3, 0.75, 0.25).normalize(),
            color: Vec3::new(0.55, 0.75, 1.0),
        }),
        bounds: (
            Vec3::new(-extent, floor_y, -extent),
            Vec3::new(extent, floor_y + wall_h, extent),
        ),
    }
}

/// Deep-night sky: indigo zenith, magenta glow at the horizon.
pub fn neon_sky() -> Sky {
    Sky {
        zenith: Vec3::new(0.02, 0.03, 0.09),
        horizon: Vec3::new(0.26, 0.07, 0.33),
        sun_dir: Vec3::new(0.3, 0.75, 0.25).normalize(),
        sun_color: Vec3::new(0.55, 0.75, 1.0),
    }
}

/// Cool hemisphere light with a cyan key light.
pub fn neon_lighting() -> ModelLighting {
    ModelLighting {
        sun_dir: Vec3::new(0.3, 0.75, 0.25).normalize(),
        sun_color: Vec3::new(0.55, 0.7, 1.0),
        ambient: Vec3::new(0.44, 0.44, 0.6),
    }
}

// ---------------------------------------------------------------------------
// Neon rifle (viewmodel-local space, muzzle at openstrike MUZZLE_LOCAL)
// ---------------------------------------------------------------------------

/// One texel per entry; every rifle face UVs at its texel center.
pub const NEON_GUN_COLORS: [[u8; 4]; 6] = [
    [26, 30, 44, 255],    // 0 receiver
    [15, 17, 24, 255],    // 1 barrel
    [240, 250, 255, 255], // 2 energy strip (white: picks up the team tint)
    [34, 38, 54, 255],    // 3 magazine
    [20, 22, 31, 255],    // 4 grip / stock
    [160, 240, 255, 255], // 5 sights
];

pub struct RifleBox {
    pub min: Vec3,
    pub max: Vec3,
    pub color: usize,
}

/// A slimmer, more angular silhouette than the base game's AK-ish rifle,
/// with a luminous side strip and glowing sights.
pub fn neon_rifle_boxes() -> Vec<RifleBox> {
    let b = |min: Vec3, max: Vec3, color: usize| RifleBox { min, max, color };
    vec![
        // Receiver.
        b(Vec3::new(-1.1, -1.6, -18.0), Vec3::new(1.1, 1.4, 3.0), 0),
        // Top rail.
        b(Vec3::new(-0.5, 1.4, -14.0), Vec3::new(0.5, 2.1, 1.0), 1),
        // Barrel + muzzle brake.
        b(Vec3::new(-0.4, 0.0, -30.0), Vec3::new(0.4, 0.9, -18.0), 1),
        b(Vec3::new(-0.6, -0.15, -31.5), Vec3::new(0.6, 1.1, -30.0), 4),
        // Handguard under the barrel.
        b(Vec3::new(-0.8, -1.2, -26.0), Vec3::new(0.8, 0.0, -18.0), 0),
        // Luminous energy strip along the handguard.
        b(
            Vec3::new(-0.86, -0.68, -24.0),
            Vec3::new(0.86, -0.48, -20.0),
            2,
        ),
        // Magazine, two-piece for a slight curve.
        b(Vec3::new(-0.85, -5.5, -9.0), Vec3::new(0.85, -1.6, -5.5), 3),
        b(Vec3::new(-0.8, -7.5, -8.0), Vec3::new(0.8, -5.5, -4.0), 3),
        // Pistol grip.
        b(Vec3::new(-0.8, -5.0, -0.5), Vec3::new(0.8, -1.6, 2.0), 4),
        // Stock with a tapered cheek riser.
        b(Vec3::new(-0.9, -1.8, 3.0), Vec3::new(0.9, 1.0, 11.0), 4),
        b(Vec3::new(-0.7, 1.0, 4.0), Vec3::new(0.7, 1.9, 9.5), 4),
        // Glowing sights.
        b(Vec3::new(-0.35, 2.1, -1.0), Vec3::new(0.35, 2.9, 0.0), 5),
        b(Vec3::new(-0.2, 1.1, -29.6), Vec3::new(0.2, 1.9, -29.0), 5),
    ]
}

/// Rifle in a duelist's hands: right hand on the grip by the chest, muzzle
/// forward. Matches the rifle-hold pose baked into the duelist clips. Shared
/// by the game (remote player rendering) and the screenshot rig.
pub fn held_rifle_transform(position: Vec3, yaw: f32, pitch: f32) -> pocket3d::prelude::Mat4 {
    use pocket3d::prelude::Mat4;
    Mat4::from_translation(position + Vec3::new(4.5, 14.5, -4.5))
        * Mat4::from_rotation_y(yaw)
        * Mat4::from_rotation_x(pitch.clamp(-0.65, 0.65))
        * Mat4::from_rotation_y(-0.03)
}

pub fn build_rifle(gpu: &Gpu, renderer: &Renderer) -> Arc<ModelAsset> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for rifle_box in neon_rifle_boxes() {
        add_box(
            &mut vertices,
            &mut indices,
            rifle_box.min,
            rifle_box.max,
            rifle_box.color,
        );
    }
    let pixels: Vec<_> = NEON_GUN_COLORS.into_iter().flatten().collect();
    ModelAsset::from_geometry(
        gpu,
        &renderer.model_material_layout,
        &renderer.samplers,
        "neon rifle",
        &vertices,
        &indices,
        Some((NEON_GUN_COLORS.len() as u32, 1, &pixels)),
    )
}

fn add_box(
    vertices: &mut Vec<ModelVertex>,
    indices: &mut Vec<u32>,
    min: Vec3,
    max: Vec3,
    color: usize,
) {
    let uv = [(color as f32 + 0.5) / NEON_GUN_COLORS.len() as f32, 0.5];
    let corner = |x: f32, y: f32, z: f32| {
        Vec3::new(
            if x > 0.0 { max.x } else { min.x },
            if y > 0.0 { max.y } else { min.y },
            if z > 0.0 { max.z } else { min.z },
        )
    };
    let faces = [
        (
            Vec3::X,
            [
                corner(1.0, -1.0, 1.0),
                corner(1.0, -1.0, -1.0),
                corner(1.0, 1.0, -1.0),
                corner(1.0, 1.0, 1.0),
            ],
        ),
        (
            -Vec3::X,
            [
                corner(-1.0, -1.0, -1.0),
                corner(-1.0, -1.0, 1.0),
                corner(-1.0, 1.0, 1.0),
                corner(-1.0, 1.0, -1.0),
            ],
        ),
        (
            Vec3::Y,
            [
                corner(-1.0, 1.0, 1.0),
                corner(1.0, 1.0, 1.0),
                corner(1.0, 1.0, -1.0),
                corner(-1.0, 1.0, -1.0),
            ],
        ),
        (
            -Vec3::Y,
            [
                corner(-1.0, -1.0, -1.0),
                corner(1.0, -1.0, -1.0),
                corner(1.0, -1.0, 1.0),
                corner(-1.0, -1.0, 1.0),
            ],
        ),
        (
            Vec3::Z,
            [
                corner(-1.0, -1.0, 1.0),
                corner(1.0, -1.0, 1.0),
                corner(1.0, 1.0, 1.0),
                corner(-1.0, 1.0, 1.0),
            ],
        ),
        (
            -Vec3::Z,
            [
                corner(1.0, -1.0, -1.0),
                corner(-1.0, -1.0, -1.0),
                corner(-1.0, 1.0, -1.0),
                corner(1.0, 1.0, -1.0),
            ],
        ),
    ];
    for (normal, quad) in faces {
        let base = vertices.len() as u32;
        for position in quad {
            vertices.push(ModelVertex {
                pos: position.to_array(),
                normal: normal.to_array(),
                uv,
                joints: [0; 4],
                weights: [1.0, 0.0, 0.0, 0.0],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

// ---------------------------------------------------------------------------
// HUD
// ---------------------------------------------------------------------------

use crate::protocol::{MatchPhase, PlayerSlot, PlayerSnapshot};
use pocket3d::hud::Hud;

pub struct FloaterState<'a> {
    pub text: &'a str,
    /// 0 (just spawned) .. 1 (expired).
    pub t: f32,
    pub color: [f32; 4],
}

/// Plain-data snapshot of everything the neon HUD draws. The game fills this
/// from live state each frame; the screenshot rig fills a demo state, so both
/// share this single implementation.
pub struct HudState<'a> {
    pub status: &'a str,
    pub slot: Option<PlayerSlot>,
    pub phase: MatchPhase,
    pub local: Option<PlayerSnapshot>,
    pub remote: Option<PlayerSnapshot>,
    pub fiber_released: Option<u128>,
    pub recoil: f32,
    pub reload_left: f32,
    pub hit_marker: f32,
    pub damage_flash: f32,
    pub fight_banner: f32,
    pub floaters: Vec<FloaterState<'a>>,
    pub fatal_error: Option<&'a str>,
}

/// The neon HUD: team-tinted crosshair, hit markers, damage vignette, vital
/// bars, settlement floaters, and phase banners.
pub fn draw_hud(hud: &mut Hud, state: &HudState, size: (u32, u32)) {
    hud.clear();
    let width = size.0 as f32;
    let height = size.1 as f32;
    let cx = width * 0.5;
    let cy = height * 0.5;
    let team = state.slot.map(team_color).unwrap_or(TEAM_A);
    let dim = [0.55, 0.65, 0.75, 0.85];

    // Crosshair: opens up with recoil, hidden while reloading.
    let gap = 4.0 + state.recoil * 7.0;
    if state.reload_left <= 0.0 {
        hud.crosshair(cx, cy, gap, 8.0, 2.0, [team[0], team[1], team[2], 0.9]);
    }

    // Hit marker: four dots at the diagonals; bigger and red on a kill.
    if state.hit_marker > 0.0 {
        let kill = state.remote.is_some_and(|remote| !remote.alive);
        let alpha = (state.hit_marker / 0.18).min(1.0);
        let (offset, dot, color) = if kill {
            (13.0, 5.0, [1.0, 0.35, 0.25, alpha])
        } else {
            (10.0, 3.5, [1.0, 1.0, 1.0, alpha])
        };
        for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            hud.rect(
                cx + dx * offset - dot * 0.5,
                cy + dy * offset - dot * 0.5,
                dot,
                dot,
                color,
            );
        }
    }

    // Damage flash: red vignette closing in from the edges.
    if state.damage_flash > 0.0 {
        let alpha = state.damage_flash * 0.8;
        let edge = 26.0 + (1.0 - state.damage_flash / 0.45) * 30.0;
        let color = [1.0, 0.12, 0.1, alpha];
        hud.rect(0.0, 0.0, width, edge, color);
        hud.rect(0.0, height - edge, width, edge, color);
        hud.rect(0.0, 0.0, edge, height, color);
        hud.rect(width - edge, 0.0, edge, height, color);
    }

    // Status line (connection / Fiber hold pipeline), small and dim.
    hud.text(16.0, 14.0, 1.5, dim, state.status);

    // Vitals: health bar bottom-left.
    if let Some(local) = state.local {
        let hp_frac = (local.health as f32 / 100.0).clamp(0.0, 1.0);
        let bar_x = 24.0;
        let bar_y = height - 44.0;
        let bar_w = 220.0;
        hud.text(
            bar_x,
            bar_y - 22.0,
            1.5,
            dim,
            &format!("HP {:03}", local.health),
        );
        hud.rect(bar_x, bar_y, bar_w, 10.0, [0.08, 0.1, 0.16, 0.8]);
        let hp_color = if hp_frac > 0.55 {
            [team[0], team[1], team[2], 0.95]
        } else if hp_frac > 0.25 {
            [1.0, 0.7, 0.25, 0.95]
        } else {
            [1.0, 0.25, 0.2, 0.95]
        };
        hud.rect(bar_x, bar_y, bar_w * hp_frac, 10.0, hp_color);

        // Ammo bottom-right, with reload feedback.
        let ammo_text = format!("{:02} / {:02}", local.ammo, local.reserve);
        let scale = 2.5;
        let text_w = Hud::text_width(&ammo_text, scale);
        hud.text(
            width - text_w - 24.0,
            height - 52.0,
            scale,
            [0.9, 0.95, 1.0, 0.95],
            &ammo_text,
        );
        let mag_frac = (local.ammo as f32 / 30.0).clamp(0.0, 1.0);
        hud.rect(
            width - 24.0 - 220.0,
            height - 20.0,
            220.0,
            5.0,
            [0.08, 0.1, 0.16, 0.8],
        );
        hud.rect(
            width - 24.0 - 220.0,
            height - 20.0,
            220.0 * mag_frac,
            5.0,
            [team[0], team[1], team[2], 0.9],
        );
        if state.reload_left > 0.0 {
            let pulse = 0.6 + 0.4 * (state.reload_left * 9.0).sin().abs();
            let text = "RELOADING";
            let w = Hud::text_width(text, 1.5);
            hud.text(
                width - 24.0 - 220.0 + (220.0 - w) * 0.5,
                height - 80.0,
                1.5,
                [1.0, 0.85, 0.4, pulse],
                text,
            );
        }
    }

    // Opponent vitals top-center.
    if let Some(remote) = state.remote {
        let opp = team_color(remote.slot);
        let frac = (remote.health as f32 / 100.0).clamp(0.0, 1.0);
        let bar_w = 180.0;
        hud.text_centered(
            cx,
            36.0,
            1.5,
            [opp[0], opp[1], opp[2], 0.9],
            &format!("OPPONENT {:03}", remote.health),
        );
        hud.rect(cx - bar_w * 0.5, 56.0, bar_w, 6.0, [0.08, 0.1, 0.16, 0.8]);
        hud.rect(
            cx - bar_w * 0.5,
            56.0,
            bar_w * frac,
            6.0,
            [opp[0], opp[1], opp[2], 0.9],
        );
    }

    // Fiber settlement total, top-right.
    if let Some(released) = state.fiber_released {
        let text = format!("FIBER SETTLED {released}");
        let text_width = Hud::text_width(&text, 1.5);
        hud.text(
            (width - text_width - 16.0).max(16.0),
            14.0,
            1.5,
            [0.65, 1.0, 0.75, 0.95],
            &text,
        );
    }

    // Settlement floaters rise from the bottom-center.
    for (i, floater) in state.floaters.iter().enumerate() {
        let f = floater.t.clamp(0.0, 1.0);
        let y = height - 150.0 - f * 60.0 - i as f32 * 22.0;
        let alpha = (1.0 - f) * 0.95;
        hud.text_centered(
            cx,
            y,
            1.8,
            [floater.color[0], floater.color[1], floater.color[2], alpha],
            floater.text,
        );
    }

    // Phase banners, center screen.
    match state.phase {
        MatchPhase::Waiting => {
            hud.text_centered(cx, cy - 60.0, 2.5, dim, "WAITING FOR OPPONENT");
        }
        MatchPhase::PaymentPaused { .. } => {
            hud.text_centered(cx, cy - 60.0, 2.5, [1.0, 0.8, 0.3, 0.95], "PAYMENT PAUSED");
        }
        MatchPhase::Ended { winner } => {
            let won = Some(winner) == state.slot;
            let (text, color) = if won {
                ("VICTORY", [team[0], team[1], team[2], 1.0])
            } else {
                ("DEFEAT", [1.0, 0.3, 0.25, 1.0])
            };
            hud.rect(0.0, cy - 100.0, width, 90.0, [0.02, 0.03, 0.06, 0.55]);
            hud.text_centered(cx, cy - 88.0, 5.0, color, text);
        }
        MatchPhase::Live => {
            if state.fight_banner > 0.0 {
                let alpha = (state.fight_banner / 1.5).min(1.0);
                hud.text_centered(
                    cx,
                    cy - 80.0,
                    4.0,
                    [team[0], team[1], team[2], alpha],
                    "FIGHT",
                );
            }
        }
    }

    if let Some(error) = state.fatal_error {
        hud.text_centered(cx, height * 0.33, 2.0, [1.0, 0.3, 0.3, 1.0], error);
    }
}
