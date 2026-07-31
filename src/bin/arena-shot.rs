//! Headless screenshot rig for the neon arena. Renders the dev arena, the
//! procedural duelist (all four animation clips), and the neon rifle into
//! offscreen PNGs — no window, no server, no GPU display required.
//!
//! ```sh
//! cargo run --features desktop --bin arena-shot -- --out shots/arena
//! ```

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use openstrike_fiber_arena::neon;
use openstrike_fiber_arena::protocol::{MatchPhase, PlayerSlot, PlayerSnapshot};
use pocket3d::{
    gpu::{Gpu, OFFSCREEN_FORMAT, OffscreenTarget},
    hud::Hud,
    model::{ModelAsset, ModelInstance},
    prelude::*,
    renderer::Renderer,
    scene::Scene,
    world::WorldModel,
};

#[derive(Debug, Parser)]
#[command(about = "Render neon arena screenshots offscreen")]
struct Args {
    /// Output path prefix; view names are appended.
    #[arg(long, default_value = "shots/arena")]
    out: PathBuf,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    #[arg(long)]
    soldier_model: Option<PathBuf>,
}

fn duelist_path(arg: Option<PathBuf>) -> PathBuf {
    arg.unwrap_or_else(neon::default_duelist_model)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let gpu = Gpu::new_headless().context("creating headless GPU")?;
    let mut renderer = Renderer::new(&gpu, OFFSCREEN_FORMAT)?;
    let target = OffscreenTarget::new(&gpu, args.width, args.height);

    let map = neon::development_map();
    let world = Arc::new(WorldModel::from_bsp(
        &gpu,
        &renderer.world_material_layout,
        &renderer.samplers,
        &map,
    ));
    let duelist = ModelAsset::load_glb(
        &gpu,
        &renderer.model_material_layout,
        &renderer.samplers,
        &duelist_path(args.soldier_model.clone()),
    )?;
    let rifle = neon::build_rifle(&gpu, &renderer);
    let clip = |name: &str| duelist.clip_named(name).unwrap_or(0);

    // Shot 1: first-person from spawn A, looking down the lane at mid, with
    // the full neon HUD driven through a demo state (same draw call the game
    // uses every frame).
    {
        let mut scene = base_scene(world.clone());
        scene.viewmodel = Some({
            let mut model = ModelInstance::new(rifle.clone());
            model.tint = neon::TEAM_A;
            model.transform = Mat4::from_translation(Vec3::new(0.0, 28.0, 260.0))
                * Mat4::from_rotation_y(0.12)
                * Mat4::from_rotation_x(-0.02)
                * Mat4::from_translation(Vec3::new(7.2, -7.0, -8.5))
                * Mat4::from_rotation_y(-0.03);
            model
        });
        // Opponent mid-arena, running across the lane.
        scene.models.push(duelist_at(
            &duelist,
            Vec3::new(60.0, 0.0, 40.0),
            std::f32::consts::PI * 0.5,
            clip("Run"),
            0.3,
            neon::TEAM_B,
            true,
        ));
        // Local tracer + muzzle flash down the lane.
        scene.sprites.push(Sprite {
            pos: Vec3::new(7.0, 21.0, 220.0),
            size: 22.0,
            color: [0.45, 0.9, 1.0, 0.85],
        });
        scene.beams.push(Beam {
            a: Vec3::new(7.0, 21.0, 220.0),
            b: Vec3::new(-4.0, 24.0, -40.0),
            width: 1.6,
            color: [0.45, 0.9, 1.0, 0.65],
        });
        let camera = Camera {
            pos: Vec3::new(0.0, 28.0, 260.0),
            yaw: 0.12,
            pitch: -0.02,
            fov_y: 74f32.to_radians(),
            ..Default::default()
        };
        let demo = |health: u16, ammo: u32, reserve: u32, alive: bool, slot| PlayerSnapshot {
            slot,
            position: [0.0; 3],
            velocity: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
            on_ground: true,
            health,
            alive,
            ammo,
            reserve,
            reloading: false,
            last_input_sequence: 0,
        };
        let mut hud = Hud::default();
        neon::draw_hud(
            &mut hud,
            &neon::HudState {
                status: "LIVE — FIBER SETTLED",
                slot: Some(PlayerSlot::A),
                phase: MatchPhase::Live,
                local: Some(demo(66, 23, 90, true, PlayerSlot::A)),
                remote: Some(demo(34, 30, 90, true, PlayerSlot::B)),
                fiber_released: Some(2000),
                recoil: 0.35,
                reload_left: 0.0,
                hit_marker: 0.15,
                damage_flash: 0.0,
                fight_banner: 0.0,
                floaters: vec![
                    neon::FloaterState {
                        text: "FIBER +1000 SHANNON",
                        t: 0.25,
                        color: [0.55, 1.0, 0.7, 1.0],
                    },
                    neon::FloaterState {
                        text: "FIBER +1000 SHANNON",
                        t: 0.7,
                        color: [0.55, 1.0, 0.7, 1.0],
                    },
                ],
                fatal_error: None,
            },
            (args.width, args.height),
        );
        render(&gpu, &mut renderer, &target, &scene, &camera, &hud);
        target.save_png(
            &gpu,
            &args.out.with_file_name(format!(
                "{}-fp.png",
                args.out.file_name().unwrap().to_string_lossy()
            )),
        )?;
    }

    // Shot 2: elevated overview of the whole arena with both duelists.
    {
        let mut scene = base_scene(world.clone());
        scene.models.push(duelist_at(
            &duelist,
            neon::devmap_spawn(0),
            0.0,
            clip("Idle"),
            0.5,
            neon::TEAM_A,
            true,
        ));
        scene.models.push(duelist_at(
            &duelist,
            neon::devmap_spawn(1),
            std::f32::consts::PI,
            clip("Walk"),
            0.25,
            neon::TEAM_B,
            true,
        ));
        let camera = Camera {
            pos: Vec3::new(-330.0, 300.0, 430.0),
            yaw: -0.65,
            pitch: -0.55,
            fov_y: 60f32.to_radians(),
            ..Default::default()
        };
        render(
            &gpu,
            &mut renderer,
            &target,
            &scene,
            &camera,
            &Hud::default(),
        );
        target.save_png(
            &gpu,
            &args.out.with_file_name(format!(
                "{}-overview.png",
                args.out.file_name().unwrap().to_string_lossy()
            )),
        )?;
    }

    // Shot 3: character lineup — Idle, Walk, Run, Death, facing the camera.
    {
        let mut scene = base_scene(world.clone());
        let lineup = [
            ("Idle", 0.5, true),
            ("Walk", 0.4, true),
            ("Run", 0.3, true),
            ("Death", 1.05, false),
        ];
        for (i, (name, time, looping)) in lineup.iter().enumerate() {
            let mut model = duelist_at(
                &duelist,
                Vec3::new(-90.0 + i as f32 * 60.0, 0.0, 150.0),
                std::f32::consts::PI,
                clip(name),
                *time,
                neon::TEAM_A,
                *looping,
            );
            if i % 2 == 1 {
                model.tint = neon::TEAM_B;
            }
            scene.models.push(model);
        }
        // The Idle duelist holds the rifle via the shared in-hands transform.
        let mut rifle_model = ModelInstance::new(rifle.clone());
        rifle_model.tint = neon::TEAM_A;
        rifle_model.transform =
            neon::held_rifle_transform(Vec3::new(-90.0, 0.0, 150.0), std::f32::consts::PI, 0.0);
        scene.models.push(rifle_model);
        let camera = Camera {
            pos: Vec3::new(0.0, 50.0, 300.0),
            yaw: 0.0,
            pitch: -0.02,
            fov_y: 50f32.to_radians(),
            ..Default::default()
        };
        render(
            &gpu,
            &mut renderer,
            &target,
            &scene,
            &camera,
            &Hud::default(),
        );
        target.save_png(
            &gpu,
            &args.out.with_file_name(format!(
                "{}-duelist.png",
                args.out.file_name().unwrap().to_string_lossy()
            )),
        )?;
    }

    // Shot 4: death-pose close-up, side view.
    {
        let mut scene = base_scene(world.clone());
        scene.models.push(duelist_at(
            &duelist,
            Vec3::new(0.0, 0.0, 150.0),
            std::f32::consts::PI * 0.5,
            clip("Death"),
            1.1,
            neon::TEAM_B,
            false,
        ));
        let camera = Camera {
            pos: Vec3::new(40.0, 10.0, 195.0),
            yaw: 0.35,
            pitch: -0.32,
            fov_y: 45f32.to_radians(),
            ..Default::default()
        };
        render(
            &gpu,
            &mut renderer,
            &target,
            &scene,
            &camera,
            &Hud::default(),
        );
        target.save_png(
            &gpu,
            &args.out.with_file_name(format!(
                "{}-death.png",
                args.out.file_name().unwrap().to_string_lossy()
            )),
        )?;
    }

    println!("screenshots written under {}", args.out.display());
    Ok(())
}

fn base_scene(world: Arc<WorldModel>) -> Scene {
    Scene {
        sky: neon::neon_sky(),
        lighting: neon::neon_lighting(),
        world: Some(world),
        ..Default::default()
    }
}

fn duelist_at(
    asset: &Arc<ModelAsset>,
    position: Vec3,
    yaw: f32,
    clip: usize,
    time: f32,
    tint: [f32; 4],
    looping: bool,
) -> ModelInstance {
    let mut model = ModelInstance::new(asset.clone());
    let scale = 70.0 / asset.height();
    model.transform = Mat4::from_translation(position - Vec3::Y * 36.0)
        * Mat4::from_rotation_y(yaw)
        * Mat4::from_scale(Vec3::splat(scale));
    model.anim = AnimState {
        clip,
        time,
        speed: 1.0,
        looping,
    };
    model.tint = tint;
    model
}

fn render(
    gpu: &Gpu,
    renderer: &mut Renderer,
    target: &OffscreenTarget,
    scene: &Scene,
    camera: &Camera,
    hud: &Hud,
) {
    renderer.render(gpu, &target.view, target.size, scene, camera, hud);
}
