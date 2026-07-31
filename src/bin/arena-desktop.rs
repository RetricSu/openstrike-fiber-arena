use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use openstrike_core::Player;
use openstrike_fiber_arena::{
    TICK_HZ,
    client::SettlementGuard,
    devmap,
    fiber::{FiberCurrency, FiberRpcClient, HoldInvoiceExpectation},
    matchmaking::{EnterRoomRequest, MatchmakerClient},
    neon,
    net::{unix_ms, unix_time},
    protocol::{
        ClientMessage, HoldInvoiceAck, HoldInvoiceFailure, HoldInvoiceOffer, HoldInvoiceRelease,
        HoldInvoiceStage, HoldInvoiceTerm, InputFrame, MatchPhase, MatchTerms, PlayerSlot,
        PlayerSnapshot, ServerMessage, decode, encode,
    },
    security::{client_authentication, client_authentication_with_token},
};
use pocket3d::{
    app::{AppConfig, Game, run},
    bsp::{Hull, MapCollision, MapData},
    input::Input,
    model::ModelAsset,
    prelude::*,
    winit::{event::MouseButton, keyboard::KeyCode},
};
use renet::{ConnectionConfig, DefaultChannel, RenetClient};
use renet_netcode::{ConnectToken, NetcodeClientTransport};

const DT: f32 = 1.0 / TICK_HZ as f32;
const PLAYER_MODEL_HEIGHT: f32 = 70.0;
const LOCAL_FIRE_INTERVAL: f32 = 0.105;

#[derive(Debug, Parser)]
#[command(about = "Native OpenStrike 1v1 client with Fiber settlement")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5000")]
    server: SocketAddr,
    /// Local UDP address. Set this to a physical-interface address when a
    /// system-wide TUN proxy does not pass game UDP traffic.
    #[arg(long, default_value = "0.0.0.0:0")]
    local_bind: SocketAddr,
    #[arg(long)]
    name: String,
    #[arg(long, conflicts_with = "dev_unsecure")]
    connect_token: Option<PathBuf>,
    /// HTTP room service. Omit --room to create a room and print its code.
    #[arg(long, conflicts_with_all = ["connect_token", "dev_unsecure"])]
    matchmaker: Option<String>,
    /// Join an existing room code; requires --matchmaker.
    #[arg(long, requires = "matchmaker")]
    room: Option<String>,
    #[arg(long, default_value_t = 300)]
    matchmaking_timeout_seconds: u64,
    #[arg(long, default_value_t = false)]
    dev_unsecure: bool,
    /// GoldSrc BSP rendered locally. It must be the same map used by the
    /// authoritative server.
    #[arg(long, required_unless_present = "dev_arena")]
    map: Option<PathBuf>,
    /// Use the built-in asset-free floor. The server must also use
    /// `--dev-arena`.
    #[arg(long, default_value_t = false, conflicts_with = "map")]
    dev_arena: bool,
    #[arg(long = "wad-dir")]
    wad_dirs: Vec<PathBuf>,
    #[arg(long, default_value_t = 1600)]
    width: u32,
    #[arg(long, default_value_t = 900)]
    height: u32,
    #[arg(long, default_value_t = false, conflicts_with = "fiber_rpc")]
    mock_payments: bool,
    #[arg(long, env = "FIBER_RPC_URL")]
    fiber_rpc: Option<String>,
    #[arg(long, env = "FIBER_CURRENCY", default_value = "Fibt")]
    fiber_currency: FiberCurrency,
    #[arg(long)]
    soldier_model: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    exit_on_end: bool,
    #[arg(long, default_value_t = false)]
    auto_fire: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if !args.mock_payments && args.fiber_rpc.is_none() {
        bail!("choose --mock-payments or configure --fiber-rpc");
    }
    if args.matchmaker.is_some() && args.mock_payments {
        bail!("HTTP matchmaking requires a real local FNN; --mock-payments is not allowed");
    }
    if args.matchmaker.is_some() && args.matchmaking_timeout_seconds == 0 {
        bail!("--matchmaking-timeout-seconds must be positive");
    }
    let connection = resolve_connection(&args)?;

    let map = match &args.map {
        Some(path) => {
            log::info!("loading map {}", path.display());
            pocket3d::bsp::load_map(path, &args.wad_dirs)
                .with_context(|| format!("loading map {}", path.display()))?
        }
        None => neon::development_map(),
    };
    let spawn = map
        .ct_spawns
        .first()
        .or_else(|| map.t_spawns.first())
        .copied()
        .context("map has no player spawn")?;
    let soldier_model = args
        .soldier_model
        .clone()
        .unwrap_or_else(neon::default_duelist_model);
    let title = format!("OpenStrike Fiber Arena — {}", args.name);
    let game = DesktopGame::connect(&args, connection, map, spawn.pos, spawn.yaw, soldier_model)?;
    run(
        AppConfig {
            title,
            size: (args.width, args.height),
            tick_hz: TICK_HZ as f32,
            capture_mouse: true,
        },
        game,
    )
}

struct ConnectionSetup {
    server: SocketAddr,
    connect_token: Option<ConnectToken>,
}

fn resolve_connection(args: &Args) -> Result<ConnectionSetup> {
    let Some(matchmaker_url) = &args.matchmaker else {
        return Ok(ConnectionSetup {
            server: args.server,
            connect_token: None,
        });
    };
    let fiber_rpc = args
        .fiber_rpc
        .as_deref()
        .context("--matchmaker requires --fiber-rpc or FIBER_RPC_URL")?;
    let runtime = tokio::runtime::Runtime::new().context("creating matchmaking runtime")?;
    runtime.block_on(async {
        let node = FiberRpcClient::new(fiber_rpc)
            .node_info()
            .await
            .context("reading the local FNN identity for matchmaking")?;
        let client = MatchmakerClient::new(matchmaker_url)?;
        let request = EnterRoomRequest {
            name: args.name.clone(),
            fiber_pubkey: node.pubkey,
        };
        let initial = match &args.room {
            Some(room) => client.join_room(room, &request).await?,
            None => client.create_room(&request).await?,
        };
        println!("ROOM CODE: {}", initial.room_code);
        log::info!(
            "room {} assigned slot {:?}; waiting for opponent",
            initial.room_code,
            initial.slot
        );
        let room_code = initial.room_code.clone();
        let ticket_secret = initial.ticket.clone();
        let ready = match client
            .wait_until_ready(
                initial,
                Duration::from_secs(args.matchmaking_timeout_seconds),
            )
            .await
        {
            Ok(ready) => ready,
            Err(error) => {
                let _ = client.leave(&room_code, &ticket_secret).await;
                return Err(error);
            }
        };
        let (server, connect_token) = ready.ready_connection()?;
        log::info!("room {} ready at {}", ready.room_code, server);
        Ok(ConnectionSetup {
            server,
            connect_token: Some(connect_token),
        })
    })
}

struct Predictor {
    player: Player,
    pending: VecDeque<InputFrame>,
    initialized: bool,
    dev_arena: bool,
}

impl Predictor {
    fn new(position: Vec3, yaw: f32, dev_arena: bool) -> Self {
        let mut player = Player::spawn(position, yaw);
        if dev_arena {
            player.params.gravity = 0.0;
            player.state.on_ground = true;
        }
        Self {
            player,
            pending: VecDeque::new(),
            initialized: false,
            dev_arena,
        }
    }

    fn push(&mut self, input: InputFrame, collision: &MapCollision) {
        self.pending.push_back(input);
        predict_step(&mut self.player, collision, input, self.dev_arena);
    }

    fn reconcile(&mut self, snapshot: PlayerSnapshot, collision: &MapCollision) {
        let position = Vec3::from(snapshot.position);
        self.player.prev_pos = position;
        self.player.state.pos = position;
        self.player.state.vel = Vec3::from(snapshot.velocity);
        self.player.state.on_ground = snapshot.on_ground;
        if self.dev_arena {
            self.player.state.on_ground = true;
            self.player.state.vel.y = 0.0;
        }
        self.player.yaw = snapshot.yaw;
        self.player.pitch = snapshot.pitch;
        self.player.health = snapshot.health as i32;
        self.player.alive = snapshot.alive;
        self.pending
            .retain(|input| input.sequence > snapshot.last_input_sequence);
        for input in self.pending.iter().copied() {
            predict_step(&mut self.player, collision, input, self.dev_arena);
        }
        self.initialized = true;
    }
}

fn predict_step(player: &mut Player, collision: &MapCollision, input: InputFrame, dev_arena: bool) {
    if !player.alive {
        return;
    }
    let input = input.sanitized();
    player.prev_pos = player.state.pos;
    player.yaw = input.yaw;
    player.pitch = input.pitch;
    let wish = player.forward_flat() * input.move_y + player.right() * input.move_x;
    if dev_arena {
        devmap::flat_step(player, wish, input.walk, DT);
        return;
    }
    pocket3d::collide::step_character(
        collision,
        pocket3d::collide::HullKind::Stand,
        &mut player.state,
        &player.params,
        &pocket3d::collide::MoveInput {
            wish_dir: wish,
            speed: if input.walk {
                openstrike_core::sim::WALK_SPEED_SCALE
            } else {
                1.0
            },
            jump: input.jump,
        },
        DT,
    );
}

#[derive(Default)]
struct RemoteTrack {
    previous: Option<PlayerSnapshot>,
    current: Option<PlayerSnapshot>,
    animation_clip: Option<usize>,
    animation_time: f32,
}

struct ShotEffect {
    age: f32,
    ttl: f32,
    a: Vec3,
    b: Vec3,
    color: [f32; 4],
}

struct Floater {
    text: String,
    age: f32,
    ttl: f32,
    color: [f32; 4],
}

enum FiberWorkerEvent {
    Message(ClientMessage),
    SetupReady {
        fnn_version: String,
        channel_id: String,
        outbound: u128,
        invoices: Vec<(HoldInvoiceTerm, String)>,
    },
    SetupFailed(String),
}

impl RemoteTrack {
    fn update(&mut self, snapshot: PlayerSnapshot) {
        self.previous = self.current.or(Some(snapshot));
        self.current = Some(snapshot);
    }

    fn advance_animation(
        &mut self,
        dt: f32,
        idle_clip: usize,
        walk_clip: usize,
        run_clip: usize,
        death_clip: Option<usize>,
    ) {
        let Some(player) = self.current else {
            return;
        };
        if !player.alive {
            if let Some(death) = death_clip {
                if self.animation_clip != Some(death) {
                    self.animation_clip = Some(death);
                    self.animation_time = 0.0;
                } else {
                    // Non-looping: the skeleton sampler clamps at the last key.
                    self.animation_time += dt;
                }
            } else {
                self.animation_clip = Some(idle_clip);
                self.animation_time = 0.0;
            }
            return;
        }

        let velocity = Vec3::from(player.velocity);
        let horizontal_speed = Vec3::new(velocity.x, 0.0, velocity.z).length();
        let (clip, playback_rate) = if horizontal_speed < 8.0 {
            (idle_clip, 1.0)
        } else if horizontal_speed < 180.0 {
            (walk_clip, (horizontal_speed / 120.0).clamp(0.55, 1.6))
        } else {
            (run_clip, (horizontal_speed / 235.0).clamp(0.75, 1.35))
        };
        if self.animation_clip != Some(clip) {
            self.animation_clip = Some(clip);
            self.animation_time = 0.0;
        }
        self.animation_time += dt * playback_rate;
    }
}

struct DesktopGame {
    player_name: String,
    map: MapData,
    dev_arena: bool,
    scene: Scene,
    camera: Camera,
    hud: Hud,
    rifle_asset: Option<Arc<ModelAsset>>,
    soldier_asset: Option<Arc<ModelAsset>>,
    soldier_model_path: PathBuf,
    idle_clip: usize,
    walk_clip: usize,
    run_clip: usize,
    death_clip: Option<usize>,
    predictor: Predictor,
    remote: RemoteTrack,
    local_snapshot: Option<PlayerSnapshot>,
    network: RenetClient,
    transport: NetcodeClientTransport,
    slot: Option<PlayerSlot>,
    guard: Option<SettlementGuard>,
    running: bool,
    phase: MatchPhase,
    input_sequence: u32,
    client_tick: u64,
    view_yaw: f32,
    view_pitch: f32,
    status: String,
    mock_payments: bool,
    fiber: Option<FiberRpcClient>,
    fiber_currency: FiberCurrency,
    payment_runtime: Option<tokio::runtime::Runtime>,
    payment_tx: mpsc::Sender<FiberWorkerEvent>,
    payment_rx: mpsc::Receiver<FiberWorkerEvent>,
    pending_operations: usize,
    match_end_received: bool,
    exit_on_end: bool,
    auto_fire: bool,
    local_fire_cooldown: f32,
    local_recoil: f32,
    shot_effects: Vec<ShotEffect>,
    fatal_error: Option<String>,
    // Neon presentation state.
    sway: Vec2,
    sway_input: Vec2,
    bob_phase: f32,
    bob_amount: f32,
    reload_left: f32,
    hit_marker: f32,
    damage_flash: f32,
    fight_banner: f32,
    floaters: Vec<Floater>,
}

impl DesktopGame {
    fn connect(
        args: &Args,
        connection: ConnectionSetup,
        map: MapData,
        spawn_position: Vec3,
        spawn_yaw: f32,
        soldier_model_path: PathBuf,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(args.local_bind)
            .with_context(|| format!("binding client UDP socket to {}", args.local_bind))?;
        let client_id = unix_time().as_nanos().min(u64::MAX as u128) as u64;
        let auth = match connection.connect_token {
            Some(token) => client_authentication_with_token(
                Some(token),
                false,
                connection.server,
                client_id,
                &args.name,
            )?,
            None => client_authentication(
                args.connect_token.as_deref(),
                args.dev_unsecure,
                connection.server,
                client_id,
                &args.name,
            )?,
        };
        let transport = NetcodeClientTransport::new(unix_time(), auth, socket)?;
        let network = RenetClient::new(ConnectionConfig::default());
        let fiber = args.fiber_rpc.as_ref().map(FiberRpcClient::new);
        let payment_runtime = if args.mock_payments {
            None
        } else {
            Some(tokio::runtime::Runtime::new().context("creating Fiber payment runtime")?)
        };
        let (payment_tx, payment_rx) = mpsc::channel();

        log::info!("connecting to {}", connection.server);
        let predictor = Predictor::new(spawn_position, spawn_yaw, args.dev_arena);
        Ok(Self {
            player_name: args.name.clone(),
            map,
            dev_arena: args.dev_arena,
            scene: Scene::default(),
            camera: Camera {
                fov_y: 74f32.to_radians(),
                ..Default::default()
            },
            hud: Hud::default(),
            rifle_asset: None,
            soldier_asset: None,
            soldier_model_path,
            idle_clip: 0,
            walk_clip: 0,
            run_clip: 0,
            death_clip: None,
            predictor,
            remote: RemoteTrack::default(),
            local_snapshot: None,
            network,
            transport,
            slot: None,
            guard: None,
            running: false,
            phase: MatchPhase::Waiting,
            input_sequence: 0,
            client_tick: 0,
            view_yaw: spawn_yaw,
            view_pitch: 0.0,
            status: "CONNECTING".into(),
            mock_payments: args.mock_payments,
            fiber,
            fiber_currency: args.fiber_currency,
            payment_runtime,
            payment_tx,
            payment_rx,
            pending_operations: 0,
            match_end_received: false,
            exit_on_end: args.exit_on_end,
            auto_fire: args.auto_fire,
            local_fire_cooldown: 0.0,
            local_recoil: 0.0,
            shot_effects: Vec::new(),
            fatal_error: None,
            sway: Vec2::ZERO,
            sway_input: Vec2::ZERO,
            bob_phase: 0.0,
            bob_amount: 0.0,
            reload_left: 0.0,
            hit_marker: 0.0,
            damage_flash: 0.0,
            fight_banner: 0.0,
            floaters: Vec::new(),
        })
    }

    fn update_network(&mut self, dt: f32, input: &Input) {
        let elapsed = Duration::from_secs_f32(dt);
        self.network.update(elapsed);
        if let Err(error) = self.transport.update(elapsed, &mut self.network) {
            self.fail(format!("transport update failed: {error}"));
            return;
        }

        while let Some(bytes) = self
            .network
            .receive_message(DefaultChannel::ReliableOrdered)
        {
            match decode::<ServerMessage>(&bytes) {
                Ok(message) => self.handle_server_message(message),
                Err(error) => log::warn!("invalid server control message: {error}"),
            }
        }
        while let Some(bytes) = self.network.receive_message(DefaultChannel::Unreliable) {
            if let Ok(ServerMessage::Snapshot(snapshot)) = decode(&bytes) {
                self.apply_snapshot(snapshot);
            }
        }
        self.drain_fiber_results();

        if self.running
            && self.phase == MatchPhase::Live
            && self.predictor.initialized
            && self.network.is_connected()
        {
            self.client_tick = self.client_tick.wrapping_add(1);
            self.input_sequence = self.input_sequence.wrapping_add(1);
            let frame = InputFrame {
                sequence: self.input_sequence,
                client_tick: self.client_tick,
                move_x: axis(input, KeyCode::KeyD, KeyCode::KeyA),
                move_y: axis(input, KeyCode::KeyW, KeyCode::KeyS),
                yaw: self.view_yaw,
                pitch: self.view_pitch,
                walk: input.key_down(KeyCode::ShiftLeft),
                jump: input.key_down(KeyCode::Space),
                fire: self.auto_fire || input.mouse_button_down(MouseButton::Left),
                reload: input.key_pressed(KeyCode::KeyR),
            };
            if frame.sequence == 1 {
                log::info!(
                    "sending first input as {:?}: yaw {:.3} pitch {:.3} fire {}",
                    self.slot,
                    frame.yaw,
                    frame.pitch,
                    frame.fire
                );
            }
            if self.should_spawn_local_shot(frame.fire) {
                self.spawn_local_shot_effect();
            }
            self.predictor.push(frame, &self.map.collision);
            match encode(&ClientMessage::Input(frame)) {
                Ok(message) => self
                    .network
                    .send_message(DefaultChannel::Unreliable, message),
                Err(error) => self.fail(format!("input encoding failed: {error}")),
            }
        }

        if let Some(reason) = self.network.disconnect_reason() {
            self.fail(format!("disconnected: {reason}"));
        }
        if let Err(error) = self.transport.send_packets(&mut self.network) {
            self.fail(format!("sending packets failed: {error}"));
        }
    }

    fn handle_server_message(&mut self, message: ServerMessage) {
        match message {
            ServerMessage::Welcome { slot, terms } => {
                let match_id = terms.match_id;
                if let Err(error) = self.prepare_hold_invoices(slot, &terms) {
                    self.fail(format!("Fiber hold-invoice setup failed: {error:#}"));
                    return;
                }
                self.slot = Some(slot);
                self.guard = Some(SettlementGuard::new(terms));
                self.status = if self.mock_payments {
                    format!("LOCKING HOLDS AS {slot:?}")
                } else {
                    "CHECKING FIBER CHANNEL".into()
                };
                self.send_control(ClientMessage::AcceptTerms { match_id });
                log::info!("joined bound match {match_id} as {slot:?}");
            }
            ServerMessage::HoldInvoiceOffer(offer) => self.handle_invoice_offer(offer),
            ServerMessage::MatchStarted { match_id } => {
                self.running = true;
                self.status = "LIVE".into();
                log::info!("match {match_id} started after all holds reached Received");
            }
            ServerMessage::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            ServerMessage::HoldInvoiceRelease(release) => self.handle_invoice_release(release),
            ServerMessage::CancelHoldInvoice {
                match_id,
                reservation_id,
                payment_hash,
            } => self.handle_invoice_cancel(match_id, reservation_id, payment_hash),
            ServerMessage::MatchEnded { match_id, winner } => {
                self.running = false;
                self.match_end_received = true;
                self.status = format!("MATCH ENDED — {winner:?} WON");
                log::info!("match {match_id} ended; winner {winner:?}");
            }
            ServerMessage::Error(message) => self.fail(format!("server error: {message}")),
        }
    }

    fn prepare_hold_invoices(&mut self, slot: PlayerSlot, terms: &MatchTerms) -> Result<()> {
        let binding = &terms.players[slot.index()];
        ensure!(
            binding.name == self.player_name,
            "connect token is bound to {}, not {}",
            binding.name,
            self.player_name
        );

        if self.mock_payments {
            for term in terms.hold_invoices.iter().filter(|term| term.payee == slot) {
                self.send_control(ClientMessage::HoldInvoiceOffer(HoldInvoiceOffer {
                    match_id: terms.match_id,
                    reservation_id: term.reservation_id,
                    payment_hash: term.payment_hash,
                    invoice: format!(
                        "mock:{}:{}:{}",
                        terms.match_id,
                        term.reservation_id,
                        hex::encode(term.payment_hash)
                    ),
                }));
                self.send_control(ClientMessage::HoldInvoiceAck(hold_ack(
                    terms.match_id,
                    term,
                    HoldInvoiceStage::Received,
                )));
            }
            return Ok(());
        }

        let rpc = self.fiber.clone().context("Fiber RPC unavailable")?;
        let handle = self
            .payment_runtime
            .as_ref()
            .context("Fiber runtime unavailable")?
            .handle()
            .clone();
        let currency = self.fiber_currency;
        let local_pubkey = binding.fiber_pubkey.clone();
        let opponent_pubkey = terms.players[slot.opponent().index()].fiber_pubkey.clone();
        let payee_terms: Vec<_> = terms
            .hold_invoices
            .iter()
            .filter(|term| term.payee == slot)
            .cloned()
            .collect();
        let terms = terms.clone();
        let tx = self.payment_tx.clone();
        self.pending_operations += 1;
        self.status = "CHECKING FIBER CHANNEL".into();
        handle.spawn(async move {
            let created: std::result::Result<_, openstrike_fiber_arena::fiber::FiberError> =
                async {
                    let readiness = rpc
                        .check_direct_channel(&opponent_pubkey, terms.max_total_per_player)
                        .await?;
                    if readiness.node.pubkey != local_pubkey {
                        return Err(openstrike_fiber_arena::fiber::FiberError::InvoiceMismatch(
                            format!(
                                "local FNN pubkey {} differs from connect-token binding {}",
                                readiness.node.pubkey, local_pubkey
                            ),
                        ));
                    }
                    let mut invoices = Vec::with_capacity(payee_terms.len());
                    for term in payee_terms {
                        let expectation =
                            HoldInvoiceExpectation::new(&terms, &term, &local_pubkey, currency)?;
                        let invoice = rpc.create_hold_invoice(&expectation).await?;
                        invoices.push((term, invoice));
                    }
                    Ok((readiness, invoices))
                }
                .await;
            let event = match created {
                Ok((readiness, invoices)) => FiberWorkerEvent::SetupReady {
                    fnn_version: readiness.node.version,
                    channel_id: readiness.channel.channel_id,
                    outbound: readiness.channel.local_balance,
                    invoices,
                },
                Err(error) => FiberWorkerEvent::SetupFailed(error.to_string()),
            };
            tx.send(event).ok();
        });
        Ok(())
    }

    fn wait_for_invoice_received(&mut self, term: HoldInvoiceTerm) -> Result<()> {
        let rpc = self.fiber.clone().context("Fiber RPC unavailable")?;
        let handle = self
            .payment_runtime
            .as_ref()
            .context("Fiber runtime unavailable")?
            .handle()
            .clone();
        let terms = self
            .guard
            .as_ref()
            .context("settlement terms unavailable")?
            .terms();
        let match_id = terms.match_id;
        let timeout = Duration::from_millis(terms.payment_deadline_ms);
        let tx = self.payment_tx.clone();
        self.pending_operations += 1;
        handle.spawn(async move {
            let message = match rpc.wait_invoice_received(term.payment_hash, timeout).await {
                Ok(()) => ClientMessage::HoldInvoiceAck(hold_ack(
                    match_id,
                    &term,
                    HoldInvoiceStage::Received,
                )),
                Err(error) => ClientMessage::HoldInvoiceFailure(hold_failure(
                    match_id,
                    &term,
                    HoldInvoiceStage::Received,
                    error.to_string(),
                )),
            };
            tx.send(FiberWorkerEvent::Message(message)).ok();
        });
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: openstrike_fiber_arena::protocol::WorldSnapshot) {
        let previous_phase = self.phase;
        self.phase = snapshot.phase;
        if previous_phase != MatchPhase::Live && self.phase == MatchPhase::Live {
            self.fight_banner = 1.5;
        }
        let Some(slot) = self.slot else {
            return;
        };
        let local = snapshot.players[slot.index()];
        let remote = snapshot.players[slot.opponent().index()];
        if !self.predictor.initialized {
            self.view_yaw = local.yaw;
            self.view_pitch = if self.auto_fire { -0.05 } else { local.pitch };
        }

        // Presentation deltas, derived from consecutive snapshots.
        if let Some(previous) = self.local_snapshot
            && local.health < previous.health
        {
            self.damage_flash = 0.45;
        }
        if let Some(previous) = self.remote.current {
            if remote.health < previous.health {
                self.hit_marker = if remote.alive { 0.18 } else { 0.4 };
            }
            if remote.ammo < previous.ammo && remote.alive {
                self.spawn_remote_shot_effect(remote);
            }
        }
        if let Some(previous) = self.local_snapshot
            && !previous.reloading
            && local.reloading
        {
            self.reload_left = openstrike_core::weapon::WeaponConfig::default().reload_time;
        }

        self.predictor.reconcile(local, &self.map.collision);
        self.local_snapshot = Some(local);
        self.remote.update(remote);
    }

    fn spawn_remote_shot_effect(&mut self, remote: PlayerSnapshot) {
        let eye = Vec3::from(remote.position) + Vec3::Y * 28.0;
        let dir = view_direction(remote.yaw, remote.pitch);
        let right = Vec3::new(remote.yaw.cos(), 0.0, -remote.yaw.sin());
        let muzzle = eye + dir * 14.0 + right * 3.0 - Vec3::Y * 3.5;
        let range = self.trace_visual(eye, dir, 900.0);
        self.shot_effects.push(ShotEffect {
            age: 0.0,
            ttl: 0.09,
            a: muzzle,
            b: eye + dir * range,
            color: neon::team_color(remote.slot),
        });
    }

    /// World-hit distance for tracer endpoints: shared dev-arena collision,
    /// or the BSP collision world on real maps.
    fn trace_visual(&self, origin: Vec3, dir: Vec3, max: f32) -> f32 {
        if self.dev_arena {
            return devmap::trace_distance(origin, dir, max);
        }
        let trace = self
            .map
            .collision
            .trace(Hull::Point, origin, origin + dir * max);
        trace.fraction * max
    }

    fn handle_invoice_offer(&mut self, offer: HoldInvoiceOffer) {
        let Some(slot) = self.slot else {
            return;
        };
        let Some(guard) = self.guard.as_ref() else {
            return;
        };
        if offer.match_id != guard.terms().match_id {
            self.fail("hold invoice belongs to another match".into());
            return;
        }
        let Some(term) = guard
            .terms()
            .hold_invoices
            .iter()
            .find(|term| term.reservation_id == offer.reservation_id)
            .cloned()
        else {
            self.fail("unknown hold-invoice reservation".into());
            return;
        };
        if term.payer != slot || term.payment_hash != offer.payment_hash {
            self.fail("hold-invoice offer conflicts with bound match terms".into());
            return;
        }
        if self.mock_payments {
            self.send_control(ClientMessage::HoldInvoiceAck(hold_ack(
                guard.terms().match_id,
                &term,
                HoldInvoiceStage::Funded,
            )));
            return;
        }

        let Some(handle) = self
            .payment_runtime
            .as_ref()
            .map(|runtime| runtime.handle().clone())
        else {
            self.fail("Fiber runtime unavailable".into());
            return;
        };
        let Some(rpc) = self.fiber.clone() else {
            self.fail("Fiber RPC unavailable".into());
            return;
        };
        let terms = guard.terms().clone();
        let currency = self.fiber_currency;
        let tx = self.payment_tx.clone();
        self.pending_operations += 1;
        self.status = format!("LOCKING HOLD #{}", term.reservation_id);
        handle.spawn(async move {
            let result = async {
                let expectation = HoldInvoiceExpectation::new(
                    &terms,
                    &term,
                    &terms.players[term.payee.index()].fiber_pubkey,
                    currency,
                )?;
                rpc.fund_hold_invoice(
                    &offer.invoice,
                    &expectation,
                    terms.hold_payment_timeout_seconds,
                )
                .await
            }
            .await;
            let message = match result {
                Ok(_) => ClientMessage::HoldInvoiceAck(hold_ack(
                    terms.match_id,
                    &term,
                    HoldInvoiceStage::Funded,
                )),
                Err(error) => ClientMessage::HoldInvoiceFailure(hold_failure(
                    terms.match_id,
                    &term,
                    HoldInvoiceStage::Funded,
                    error.to_string(),
                )),
            };
            tx.send(FiberWorkerEvent::Message(message)).ok();
        });
    }

    fn handle_invoice_release(&mut self, release: HoldInvoiceRelease) {
        let Some(slot) = self.slot else {
            return;
        };
        let Some(guard) = self.guard.as_mut() else {
            return;
        };
        if let Err(error) = guard.validate_release(&release, slot, unix_ms()) {
            log::warn!(
                "refusing hold release {}: {error}",
                release.intent.body.reservation_id
            );
            self.send_control(ClientMessage::HoldInvoiceFailure(HoldInvoiceFailure {
                match_id: release.intent.body.match_id,
                reservation_id: release.intent.body.reservation_id,
                payment_hash: release.intent.body.payment_hash,
                stage: HoldInvoiceStage::Settled,
                error,
            }));
            return;
        }
        let term = guard
            .terms()
            .hold_invoices
            .iter()
            .find(|term| term.reservation_id == release.intent.body.reservation_id)
            .expect("validated hold reservation")
            .clone();
        let terms = guard.terms().clone();
        if self.mock_payments {
            self.send_control(ClientMessage::HoldInvoiceAck(hold_ack(
                terms.match_id,
                &term,
                HoldInvoiceStage::Settled,
            )));
            return;
        }
        let Some(handle) = self
            .payment_runtime
            .as_ref()
            .map(|runtime| runtime.handle().clone())
        else {
            self.fail("Fiber runtime unavailable".into());
            return;
        };
        let Some(rpc) = self.fiber.clone() else {
            self.fail("Fiber RPC unavailable".into());
            return;
        };
        let tx = self.payment_tx.clone();
        self.pending_operations += 1;
        self.status = format!("SETTLING HOLD #{}", term.reservation_id);
        handle.spawn(async move {
            let result = rpc
                .settle_hold_invoice(
                    term.payment_hash,
                    release.payment_preimage,
                    Duration::from_millis(terms.payment_deadline_ms),
                )
                .await;
            let message = match result {
                Ok(()) => ClientMessage::HoldInvoiceAck(hold_ack(
                    terms.match_id,
                    &term,
                    HoldInvoiceStage::Settled,
                )),
                Err(error) => ClientMessage::HoldInvoiceFailure(hold_failure(
                    terms.match_id,
                    &term,
                    HoldInvoiceStage::Settled,
                    error.to_string(),
                )),
            };
            tx.send(FiberWorkerEvent::Message(message)).ok();
        });
    }

    fn handle_invoice_cancel(
        &mut self,
        match_id: u128,
        reservation_id: u16,
        payment_hash: [u8; 32],
    ) {
        let Some(slot) = self.slot else {
            return;
        };
        let Some(guard) = self.guard.as_ref() else {
            return;
        };
        let Some(term) = guard
            .terms()
            .hold_invoices
            .iter()
            .find(|term| term.reservation_id == reservation_id)
            .cloned()
        else {
            self.fail("server requested cancellation of unknown hold".into());
            return;
        };
        if match_id != guard.terms().match_id
            || term.payee != slot
            || term.payment_hash != payment_hash
        {
            self.fail("invalid hold-invoice cancellation request".into());
            return;
        }
        if self.mock_payments {
            self.send_control(ClientMessage::HoldInvoiceAck(hold_ack(
                match_id,
                &term,
                HoldInvoiceStage::Cancelled,
            )));
            return;
        }
        let Some(handle) = self
            .payment_runtime
            .as_ref()
            .map(|runtime| runtime.handle().clone())
        else {
            self.fail("Fiber runtime unavailable".into());
            return;
        };
        let Some(rpc) = self.fiber.clone() else {
            self.fail("Fiber RPC unavailable".into());
            return;
        };
        let tx = self.payment_tx.clone();
        self.pending_operations += 1;
        handle.spawn(async move {
            let result = rpc.cancel_hold_invoice(term.payment_hash).await;
            let message = match result {
                Ok(()) => ClientMessage::HoldInvoiceAck(hold_ack(
                    match_id,
                    &term,
                    HoldInvoiceStage::Cancelled,
                )),
                Err(error) => ClientMessage::HoldInvoiceFailure(hold_failure(
                    match_id,
                    &term,
                    HoldInvoiceStage::Cancelled,
                    error.to_string(),
                )),
            };
            tx.send(FiberWorkerEvent::Message(message)).ok();
        });
    }

    fn drain_fiber_results(&mut self) {
        while let Ok(event) = self.payment_rx.try_recv() {
            self.pending_operations = self.pending_operations.saturating_sub(1);
            match event {
                FiberWorkerEvent::Message(message) => {
                    match &message {
                        ClientMessage::HoldInvoiceAck(ack) => {
                            log::info!(
                                "hold #{} completed stage {:?}",
                                ack.reservation_id,
                                ack.stage
                            );
                            if ack.stage == HoldInvoiceStage::Settled
                                && let Some(guard) = &self.guard
                                && let Some(term) = guard
                                    .terms()
                                    .hold_invoices
                                    .iter()
                                    .find(|term| term.reservation_id == ack.reservation_id)
                            {
                                let direction = if Some(term.payee) == self.slot {
                                    "+"
                                } else {
                                    "-"
                                };
                                self.floaters.push(Floater {
                                    text: format!("FIBER {direction}{} SHANNON", term.amount),
                                    age: 0.0,
                                    ttl: 1.6,
                                    color: [0.55, 1.0, 0.7, 1.0],
                                });
                            }
                            self.status = if self.running {
                                "LIVE - FIBER SETTLED".into()
                            } else {
                                "PREPARING FIBER HOLDS".into()
                            };
                        }
                        ClientMessage::HoldInvoiceFailure(failure) => {
                            log::warn!(
                                "hold #{} {:?} failed: {}",
                                failure.reservation_id,
                                failure.stage,
                                failure.error
                            );
                            self.status = "FIBER HOLD FAILED".into();
                        }
                        _ => {}
                    }
                    self.send_control(message);
                }
                FiberWorkerEvent::SetupReady {
                    fnn_version,
                    channel_id,
                    outbound,
                    invoices,
                } => {
                    log::info!(
                        "bound Fiber channel ready: FNN {fnn_version} channel {channel_id} outbound {outbound}"
                    );
                    self.status = "PREPARING FIBER HOLDS".into();
                    let Some(match_id) = self.guard.as_ref().map(|guard| guard.terms().match_id)
                    else {
                        self.fail("settlement terms unavailable after Fiber setup".into());
                        continue;
                    };
                    for (term, invoice) in invoices {
                        self.send_control(ClientMessage::HoldInvoiceOffer(HoldInvoiceOffer {
                            match_id,
                            reservation_id: term.reservation_id,
                            payment_hash: term.payment_hash,
                            invoice,
                        }));
                        if let Err(error) = self.wait_for_invoice_received(term) {
                            self.fail(format!("waiting for Fiber hold invoice failed: {error:#}"));
                            break;
                        }
                    }
                }
                FiberWorkerEvent::SetupFailed(error) => {
                    let failure = self.slot.and_then(|slot| {
                        self.guard.as_ref().and_then(|guard| {
                            let terms = guard.terms();
                            terms
                                .hold_invoices
                                .iter()
                                .find(|term| term.payee == slot)
                                .map(|term| {
                                    ClientMessage::HoldInvoiceFailure(hold_failure(
                                        terms.match_id,
                                        term,
                                        HoldInvoiceStage::Received,
                                        error.clone(),
                                    ))
                                })
                        })
                    });
                    if let Some(failure) = failure {
                        self.send_control(failure);
                    }
                    self.fail(format!("Fiber hold-invoice setup failed: {error}"));
                }
            }
        }
    }

    fn send_control(&mut self, message: ClientMessage) {
        match encode(&message) {
            Ok(bytes) => self
                .network
                .send_message(DefaultChannel::ReliableOrdered, bytes),
            Err(error) => self.fail(format!("control encoding failed: {error}")),
        }
    }

    fn fail(&mut self, message: String) {
        if self.fatal_error.is_none() {
            log::error!("{message}");
            self.network.disconnect();
            self.status = "NETWORK ERROR".into();
            self.fatal_error = Some(message);
        }
    }

    fn should_spawn_local_shot(&self, fire: bool) -> bool {
        fire && self.local_fire_cooldown <= 0.0
            && self
                .local_snapshot
                .is_some_and(|local| local.alive && local.ammo > 0 && !local.reloading)
    }

    fn spawn_local_shot_effect(&mut self) {
        let eye = self.predictor.player.eye();
        let dir = view_direction(self.view_yaw, self.view_pitch);
        let muzzle = viewmodel_transform(
            eye,
            self.view_yaw,
            self.view_pitch,
            self.local_recoil,
            0.0,
            Vec2::ZERO,
            Vec2::ZERO,
        )
        .transform_point3(openstrike_core::weapon::MUZZLE_LOCAL);
        let range = self.trace_visual(eye, dir, 900.0);
        let color = self.slot.map(neon::team_color).unwrap_or(neon::TEAM_A);
        self.shot_effects.push(ShotEffect {
            age: 0.0,
            ttl: 0.08,
            a: muzzle,
            b: eye + dir * range,
            color,
        });
        self.local_fire_cooldown = LOCAL_FIRE_INTERVAL;
        self.local_recoil = (self.local_recoil + 0.55).min(1.0);
    }

    fn tick_visual_effects(&mut self, dt: f32) {
        self.local_fire_cooldown = (self.local_fire_cooldown - dt).max(0.0);
        self.local_recoil = (self.local_recoil - dt * 5.0).max(0.0);
        for effect in &mut self.shot_effects {
            effect.age += dt;
        }
        self.shot_effects.retain(|effect| effect.age < effect.ttl);

        self.hit_marker = (self.hit_marker - dt).max(0.0);
        self.damage_flash = (self.damage_flash - dt).max(0.0);
        self.fight_banner = (self.fight_banner - dt).max(0.0);
        self.reload_left = (self.reload_left - dt).max(0.0);
        for floater in &mut self.floaters {
            floater.age += dt;
        }
        self.floaters.retain(|floater| floater.age < floater.ttl);

        // View-model sway (lags the view) and walk bob (driven by speed).
        let target_sway = Vec2::new(self.sway_input.x, self.sway_input.y).clamp_length_max(1.4);
        self.sway = self.sway.lerp(target_sway, (dt * 10.0).min(1.0));
        self.sway_input = Vec2::ZERO;
        let speed = Vec3::new(
            self.predictor.player.state.vel.x,
            0.0,
            self.predictor.player.state.vel.z,
        )
        .length();
        self.bob_phase += dt * (4.0 + speed * 0.055);
        self.bob_amount = speed / 250.0;
    }

    fn compose_scene(&mut self, alpha: f32, time: f32, size: (u32, u32)) {
        self.scene.time = time;
        self.camera.pos = self.predictor.player.eye_interpolated(alpha);
        self.camera.yaw = self.view_yaw;
        self.camera.pitch = self.view_pitch;

        self.scene.models.clear();
        if let (Some(asset), Some(current)) = (&self.soldier_asset, self.remote.current) {
            let previous = self.remote.previous.unwrap_or(current);
            let position = Vec3::from(previous.position)
                .lerp(Vec3::from(current.position), alpha.clamp(0.0, 1.0));
            let yaw = lerp_angle(previous.yaw, current.yaw, alpha);
            let team = neon::team_color(current.slot);
            let mut model = ModelInstance::new(asset.clone());
            let scale = PLAYER_MODEL_HEIGHT / asset.height();
            let base =
                Mat4::from_translation(position - Vec3::Y * 36.0) * Mat4::from_rotation_y(yaw);
            model.transform = if current.alive || self.death_clip.is_some() {
                // The Death clip lays the body down itself when present.
                base * Mat4::from_scale(Vec3::splat(scale))
            } else {
                base * Mat4::from_rotation_x(-std::f32::consts::FRAC_PI_2 * 0.94)
                    * Mat4::from_scale(Vec3::splat(scale))
            };
            model.anim = AnimState {
                clip: self.remote.animation_clip.unwrap_or(self.idle_clip),
                time: self.remote.animation_time,
                speed: 1.0,
                looping: current.alive,
            };
            model.tint = if current.alive {
                team
            } else {
                [team[0] * 0.4, team[1] * 0.4, team[2] * 0.4, 1.0]
            };
            self.scene.models.push(model);
            if current.alive
                && let Some(rifle) = &self.rifle_asset
            {
                let mut rifle_model = ModelInstance::new(rifle.clone());
                rifle_model.transform = neon::held_rifle_transform(position, yaw, current.pitch);
                rifle_model.tint = team;
                self.scene.models.push(rifle_model);
            }
        }

        self.scene.viewmodel = match (&self.rifle_asset, self.predictor.player.alive) {
            (Some(rifle), true) => {
                let reload_total = openstrike_core::weapon::WeaponConfig::default().reload_time;
                let reload = if self.reload_left > 0.0 {
                    1.0 - self.reload_left / reload_total
                } else {
                    0.0
                };
                let bob = Vec2::new(
                    (self.bob_phase * 0.5).sin() * 1.1,
                    -self.bob_phase.sin().abs() * 0.7,
                ) * self.bob_amount.clamp(0.0, 1.0);
                let mut model = ModelInstance::new(rifle.clone());
                model.transform = viewmodel_transform(
                    self.camera.pos,
                    self.view_yaw,
                    self.view_pitch,
                    self.local_recoil,
                    reload,
                    self.sway,
                    bob,
                );
                if let Some(slot) = self.slot {
                    model.tint = neon::team_color(slot);
                }
                Some(model)
            }
            _ => None,
        };
        self.scene.sprites.clear();
        self.scene.beams.clear();
        self.emit_shot_effects();

        let fiber_released = self.guard.as_ref().map(|guard| {
            PlayerSlot::ALL
                .into_iter()
                .map(|payer| guard.released_total(payer))
                .sum::<u128>()
        });
        let floaters = self
            .floaters
            .iter()
            .map(|floater| neon::FloaterState {
                text: &floater.text,
                t: floater.age / floater.ttl,
                color: floater.color,
            })
            .collect();
        neon::draw_hud(
            &mut self.hud,
            &neon::HudState {
                status: &self.status,
                slot: self.slot,
                phase: self.phase,
                local: self.local_snapshot,
                remote: self.remote.current,
                fiber_released,
                recoil: self.local_recoil,
                reload_left: self.reload_left,
                hit_marker: self.hit_marker,
                damage_flash: self.damage_flash,
                fight_banner: self.fight_banner,
                floaters,
                fatal_error: self.fatal_error.as_deref(),
            },
            size,
        );
    }

    fn emit_shot_effects(&mut self) {
        for effect in &self.shot_effects {
            let f = 1.0 - (effect.age / effect.ttl).clamp(0.0, 1.0);
            let [r, g, b, _] = effect.color;
            self.scene.sprites.push(Sprite {
                pos: effect.a,
                size: 16.0 + 10.0 * f,
                color: [r, g, b, 0.9 * f],
            });
            self.scene.beams.push(Beam {
                a: effect.a,
                b: effect.b,
                width: 1.6,
                color: [r, g, b, 0.7 * f],
            });
        }
    }
}

impl Game for DesktopGame {
    fn init(&mut self, gpu: &Gpu, renderer: &mut Renderer) -> Result<()> {
        if self.dev_arena {
            self.scene.sky = neon::neon_sky();
            self.scene.lighting = neon::neon_lighting();
        } else if let Some(sun) = self.map.sun {
            self.scene.sky.sun_dir = sun.dir;
            self.scene.lighting.sun_dir = sun.dir;
            self.scene.lighting.sun_color = sun.color * 0.9;
        }
        self.scene.world = Some(Arc::new(WorldModel::from_bsp(
            gpu,
            &renderer.world_material_layout,
            &renderer.samplers,
            &self.map,
        )));
        self.rifle_asset = Some(neon::build_rifle(gpu, renderer));
        let soldier = ModelAsset::load_glb(
            gpu,
            &renderer.model_material_layout,
            &renderer.samplers,
            &self.soldier_model_path,
        )
        .with_context(|| format!("loading {}", self.soldier_model_path.display()))?;
        self.idle_clip = soldier
            .clips
            .iter()
            .position(|clip| clip.name.eq_ignore_ascii_case("idle"))
            .unwrap_or(0);
        self.walk_clip = soldier
            .clips
            .iter()
            .position(|clip| clip.name.eq_ignore_ascii_case("walk"))
            .unwrap_or(self.idle_clip);
        self.run_clip = soldier
            .clips
            .iter()
            .position(|clip| clip.name.eq_ignore_ascii_case("run"))
            .unwrap_or(self.walk_clip);
        self.death_clip = soldier
            .clips
            .iter()
            .position(|clip| clip.name.eq_ignore_ascii_case("death"));
        self.soldier_asset = Some(soldier);
        Ok(())
    }

    fn frame(&mut self, _dt: f32, input: &Input) {
        let delta = input.mouse_delta();
        self.view_yaw -= delta.x * openstrike_core::sim::MOUSE_SENS;
        self.view_pitch = (self.view_pitch - delta.y * openstrike_core::sim::MOUSE_SENS)
            .clamp(-89f32.to_radians(), 89f32.to_radians());
        // Feed the view-model sway (scaled down, clamped at consumption).
        self.sway_input += Vec2::new(delta.x, delta.y) * 0.012;
    }

    fn tick(&mut self, dt: f32, input: &Input) {
        self.tick_visual_effects(dt);
        self.update_network(dt, input);
        self.remote.advance_animation(
            dt,
            self.idle_clip,
            self.walk_clip,
            self.run_clip,
            self.death_clip,
        );
    }

    fn compose(&mut self, alpha: f32, time: f32, size: (u32, u32)) -> (&Scene, &Camera, &Hud) {
        self.compose_scene(alpha, time, size);
        (&self.scene, &self.camera, &self.hud)
    }

    fn wants_exit(&self) -> bool {
        self.exit_on_end && self.match_end_received && self.pending_operations == 0
    }
}

fn hold_ack(match_id: u128, term: &HoldInvoiceTerm, stage: HoldInvoiceStage) -> HoldInvoiceAck {
    HoldInvoiceAck {
        match_id,
        reservation_id: term.reservation_id,
        payment_hash: term.payment_hash,
        stage,
    }
}

fn hold_failure(
    match_id: u128,
    term: &HoldInvoiceTerm,
    stage: HoldInvoiceStage,
    error: String,
) -> HoldInvoiceFailure {
    HoldInvoiceFailure {
        match_id,
        reservation_id: term.reservation_id,
        payment_hash: term.payment_hash,
        stage,
        error,
    }
}

fn axis(input: &Input, positive: KeyCode, negative: KeyCode) -> f32 {
    f32::from(input.key_down(positive)) - f32::from(input.key_down(negative))
}

fn lerp_angle(from: f32, to: f32, alpha: f32) -> f32 {
    let mut delta = (to - from).rem_euclid(std::f32::consts::TAU);
    if delta > std::f32::consts::PI {
        delta -= std::f32::consts::TAU;
    }
    from + delta * alpha.clamp(0.0, 1.0)
}

/// First-person rifle transform: base pose plus recoil kick, reload dip,
/// mouse sway lag, and walk bob — all additive and renderer-side only.
fn viewmodel_transform(
    eye: Vec3,
    yaw: f32,
    pitch: f32,
    recoil: f32,
    reload: f32,
    sway: Vec2,
    bob: Vec2,
) -> Mat4 {
    let dip = (reload * std::f32::consts::PI).sin();
    Mat4::from_translation(eye)
        * Mat4::from_rotation_y(yaw)
        * Mat4::from_rotation_x(pitch)
        * Mat4::from_translation(Vec3::new(
            7.2 - sway.x + bob.x,
            -7.0 + sway.y + bob.y - dip * 5.5,
            -8.5 + recoil * 2.8,
        ))
        * Mat4::from_rotation_x(recoil * 0.10 - dip * 0.55)
        * Mat4::from_rotation_z(-sway.x * 0.04 - dip * 0.25)
        * Mat4::from_rotation_y(-0.03 - sway.x * 0.015)
}

fn view_direction(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(-sy * cp, sp, -cy * cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_interpolation_takes_short_path() {
        let from = 179f32.to_radians();
        let to = (-179f32).to_radians();
        let midpoint = lerp_angle(from, to, 0.5).to_degrees();
        assert!((midpoint.abs() - 180.0).abs() < 0.01);
    }
}
