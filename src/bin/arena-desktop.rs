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
    fiber::{FiberCurrency, FiberRpcClient, HoldInvoiceExpectation},
    net::{unix_ms, unix_time},
    protocol::{
        ClientMessage, HoldInvoiceAck, HoldInvoiceFailure, HoldInvoiceOffer, HoldInvoiceRelease,
        HoldInvoiceStage, HoldInvoiceTerm, InputFrame, MatchPhase, MatchTerms, PlayerSlot,
        PlayerSnapshot, ServerMessage, decode, encode,
    },
    security::client_authentication,
};
use pocket3d::{
    app::{AppConfig, Game, run},
    bsp::{Hull, MapCollision, MapData},
    input::Input,
    model::{ModelAsset, ModelVertex},
    prelude::*,
    winit::{event::MouseButton, keyboard::KeyCode},
};
use renet::{ConnectionConfig, DefaultChannel, RenetClient};
use renet_netcode::NetcodeClientTransport;

const DT: f32 = 1.0 / TICK_HZ as f32;
const PLAYER_MODEL_HEIGHT: f32 = 70.0;
const LOCAL_FIRE_INTERVAL: f32 = 0.105;

#[derive(Debug, Parser)]
#[command(about = "Native OpenStrike 1v1 client with Fiber settlement")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5000")]
    server: SocketAddr,
    #[arg(long)]
    name: String,
    #[arg(long, conflicts_with = "dev_unsecure")]
    connect_token: Option<PathBuf>,
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

    let map = match &args.map {
        Some(path) => {
            log::info!("loading map {}", path.display());
            pocket3d::bsp::load_map(path, &args.wad_dirs)
                .with_context(|| format!("loading map {}", path.display()))?
        }
        None => development_map(),
    };
    let spawn = map
        .ct_spawns
        .first()
        .or_else(|| map.t_spawns.first())
        .copied()
        .context("map has no player spawn")?;
    let soldier_model = args.soldier_model.clone().unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/open-strike/assets/models/Soldier.glb")
    });
    let title = format!("OpenStrike Fiber Arena — {}", args.name);
    let game = DesktopGame::connect(&args, map, spawn.pos, spawn.yaw, soldier_model)?;
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
        flat_dev_arena_step(player, wish, input.walk);
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

fn flat_dev_arena_step(player: &mut Player, wish: Vec3, walk: bool) {
    let speed_scale = if walk {
        openstrike_core::sim::WALK_SPEED_SCALE
    } else {
        1.0
    };
    let velocity = wish.normalize_or_zero() * player.params.max_speed * speed_scale;
    player.state.vel = Vec3::new(velocity.x, 0.0, velocity.z);
    player.state.pos += player.state.vel * DT;
    player.state.pos.x = player.state.pos.x.clamp(-460.0, 460.0);
    player.state.pos.y = 0.0;
    player.state.pos.z = player.state.pos.z.clamp(-460.0, 460.0);
    player.state.on_ground = true;
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
}

impl RemoteTrack {
    fn update(&mut self, snapshot: PlayerSnapshot) {
        self.previous = self.current.or(Some(snapshot));
        self.current = Some(snapshot);
    }

    fn advance_animation(&mut self, dt: f32, idle_clip: usize, walk_clip: usize, run_clip: usize) {
        let Some(player) = self.current else {
            return;
        };
        if !player.alive {
            self.animation_clip = Some(idle_clip);
            self.animation_time = 0.0;
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
    payment_tx: mpsc::Sender<ClientMessage>,
    payment_rx: mpsc::Receiver<ClientMessage>,
    pending_operations: usize,
    match_end_received: bool,
    exit_on_end: bool,
    auto_fire: bool,
    local_fire_cooldown: f32,
    local_recoil: f32,
    shot_effects: Vec<ShotEffect>,
    fatal_error: Option<String>,
}

impl DesktopGame {
    fn connect(
        args: &Args,
        map: MapData,
        spawn_position: Vec3,
        spawn_yaw: f32,
        soldier_model_path: PathBuf,
    ) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").context("binding client UDP socket")?;
        let client_id = unix_time().as_nanos().min(u64::MAX as u128) as u64;
        let auth = client_authentication(
            args.connect_token.as_deref(),
            args.dev_unsecure,
            args.server,
            client_id,
            &args.name,
        )?;
        let transport = NetcodeClientTransport::new(unix_time(), auth, socket)?;
        let network = RenetClient::new(ConnectionConfig::default());
        let fiber = args.fiber_rpc.as_ref().map(FiberRpcClient::new);
        let payment_runtime = if args.mock_payments {
            None
        } else {
            Some(tokio::runtime::Runtime::new().context("creating Fiber payment runtime")?)
        };
        let (payment_tx, payment_rx) = mpsc::channel();

        log::info!("connecting to {}", args.server);
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
                self.status = format!("LOCKING HOLDS AS {slot:?}");
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
        let created = self
            .payment_runtime
            .as_ref()
            .expect("validated Fiber runtime")
            .block_on(async {
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
                        HoldInvoiceExpectation::new(terms, &term, &local_pubkey, currency)?;
                    let invoice = rpc.create_hold_invoice(&expectation).await?;
                    invoices.push((term, invoice));
                }
                Ok::<_, openstrike_fiber_arena::fiber::FiberError>((readiness, invoices))
            })?;
        log::info!(
            "bound Fiber channel ready: FNN {} channel {} outbound {}",
            created.0.node.version,
            created.0.channel.channel_id,
            created.0.channel.local_balance
        );
        for (term, invoice) in created.1 {
            self.send_control(ClientMessage::HoldInvoiceOffer(HoldInvoiceOffer {
                match_id: terms.match_id,
                reservation_id: term.reservation_id,
                payment_hash: term.payment_hash,
                invoice,
            }));
            self.pending_operations += 1;
            let rpc = rpc.clone();
            let tx = self.payment_tx.clone();
            let match_id = terms.match_id;
            let timeout = Duration::from_millis(terms.payment_deadline_ms);
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
                tx.send(message).ok();
            });
        }
        Ok(())
    }

    fn apply_snapshot(&mut self, snapshot: openstrike_fiber_arena::protocol::WorldSnapshot) {
        self.phase = snapshot.phase;
        let Some(slot) = self.slot else {
            return;
        };
        let local = snapshot.players[slot.index()];
        let remote = snapshot.players[slot.opponent().index()];
        if !self.predictor.initialized {
            self.view_yaw = local.yaw;
            self.view_pitch = if self.auto_fire { -0.05 } else { local.pitch };
        }
        self.predictor.reconcile(local, &self.map.collision);
        self.local_snapshot = Some(local);
        self.remote.update(remote);
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
            tx.send(message).ok();
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
            tx.send(message).ok();
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
            tx.send(message).ok();
        });
    }

    fn drain_fiber_results(&mut self) {
        while let Ok(message) = self.payment_rx.try_recv() {
            self.pending_operations = self.pending_operations.saturating_sub(1);
            match &message {
                ClientMessage::HoldInvoiceAck(ack) => {
                    log::info!(
                        "hold #{} completed stage {:?}",
                        ack.reservation_id,
                        ack.stage
                    );
                    self.status = if self.running {
                        "LIVE — FIBER SETTLED".into()
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
        let muzzle = viewmodel_transform(eye, self.view_yaw, self.view_pitch, self.local_recoil)
            .transform_point3(openstrike_core::weapon::MUZZLE_LOCAL);
        let trace =
            self.map
                .collision
                .trace(Hull::Point, eye, eye + dir * openstrike_core::weapon::RANGE);
        let end = if trace.fraction < 1.0 {
            trace.end
        } else {
            eye + dir * 900.0
        };
        self.shot_effects.push(ShotEffect {
            age: 0.0,
            ttl: 0.08,
            a: muzzle,
            b: end,
        });
        self.local_fire_cooldown = LOCAL_FIRE_INTERVAL;
        self.local_recoil = (self.local_recoil + 0.55).min(1.0);
        self.status = "LIVE - FIRING".into();
    }

    fn tick_visual_effects(&mut self, dt: f32) {
        self.local_fire_cooldown = (self.local_fire_cooldown - dt).max(0.0);
        self.local_recoil = (self.local_recoil - dt * 5.0).max(0.0);
        for effect in &mut self.shot_effects {
            effect.age += dt;
        }
        self.shot_effects.retain(|effect| effect.age < effect.ttl);
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
            let mut model = ModelInstance::new(asset.clone());
            let scale = PLAYER_MODEL_HEIGHT / asset.height();
            let fall = if current.alive { 0.0 } else { 1.0 };
            model.transform =
                Mat4::from_translation(position - Vec3::Y * 36.0 - Vec3::Y * fall * 2.0)
                    * Mat4::from_rotation_y(yaw)
                    * Mat4::from_rotation_x(-fall * std::f32::consts::FRAC_PI_2 * 0.94)
                    * Mat4::from_scale(Vec3::splat(scale));
            model.anim = AnimState {
                clip: self.remote.animation_clip.unwrap_or(self.idle_clip),
                time: self.remote.animation_time,
                speed: 1.0,
                looping: true,
            };
            model.tint = if current.alive {
                [1.0; 4]
            } else {
                [0.5, 0.42, 0.4, 1.0]
            };
            self.scene.models.push(model);
            if current.alive
                && let Some(rifle) = &self.rifle_asset
            {
                let mut rifle_model = ModelInstance::new(rifle.clone());
                rifle_model.transform = remote_rifle_transform(position, yaw, current.pitch);
                self.scene.models.push(rifle_model);
            }
        }

        self.scene.viewmodel = match (&self.rifle_asset, self.predictor.player.alive) {
            (Some(rifle), true) => {
                let mut model = ModelInstance::new(rifle.clone());
                model.transform = viewmodel_transform(
                    self.camera.pos,
                    self.view_yaw,
                    self.view_pitch,
                    self.local_recoil,
                );
                Some(model)
            }
            _ => None,
        };
        self.scene.sprites.clear();
        self.scene.beams.clear();
        self.emit_shot_effects();
        self.compose_hud(size);
    }

    fn emit_shot_effects(&mut self) {
        for effect in &self.shot_effects {
            let f = 1.0 - (effect.age / effect.ttl).clamp(0.0, 1.0);
            self.scene.sprites.push(Sprite {
                pos: effect.a,
                size: 18.0 + 8.0 * f,
                color: [1.0, 0.82, 0.35, 0.9 * f],
            });
            self.scene.beams.push(Beam {
                a: effect.a,
                b: effect.b,
                width: 1.8,
                color: [1.0, 0.92, 0.55, 0.75 * f],
            });
        }
    }

    fn compose_hud(&mut self, size: (u32, u32)) {
        self.hud.clear();
        let width = size.0 as f32;
        let height = size.1 as f32;
        self.hud.crosshair(
            width * 0.5,
            height * 0.5,
            4.0,
            8.0,
            2.0,
            [0.8, 1.0, 0.8, 0.9],
        );
        self.hud.text(16.0, 16.0, 2.0, [1.0; 4], &self.status);
        self.hud.text(
            16.0,
            44.0,
            1.5,
            [0.8, 0.9, 1.0, 0.9],
            &format!(
                "SLOT {:?}  PHASE {:?}{}",
                self.slot,
                self.phase,
                if self.dev_arena { "  DEV ARENA" } else { "" }
            ),
        );
        if let Some(local) = self.local_snapshot {
            self.hud.text(
                24.0,
                height - 58.0,
                2.5,
                [1.0, 0.85, 0.7, 1.0],
                &format!(
                    "HP {:03}   AMMO {:02}/{:02}",
                    local.health, local.ammo, local.reserve
                ),
            );
        }
        if let Some(remote) = self.remote.current {
            let text = format!("OPP HP {:03}", remote.health);
            let text_width = Hud::text_width(&text, 1.8);
            self.hud.text(
                (width - text_width) * 0.5,
                76.0,
                1.8,
                [1.0, 0.72, 0.55, 0.95],
                &text,
            );
        }
        if let Some(guard) = &self.guard {
            let released = PlayerSlot::ALL
                .into_iter()
                .map(|payer| guard.released_total(payer))
                .sum::<u128>();
            let text = format!(
                "FIBER RELEASED {}  OPS {}",
                released, self.pending_operations
            );
            let text_width = Hud::text_width(&text, 1.5);
            self.hud.text(
                (width - text_width - 16.0).max(16.0),
                16.0,
                1.5,
                [0.65, 1.0, 0.75, 0.95],
                &text,
            );
        }
        if let Some(error) = &self.fatal_error {
            self.hud
                .text_centered(width * 0.5, height * 0.33, 2.0, [1.0, 0.3, 0.3, 1.0], error);
        }
    }
}

impl Game for DesktopGame {
    fn init(&mut self, gpu: &Gpu, renderer: &mut Renderer) -> Result<()> {
        if let Some(sun) = self.map.sun {
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
        self.rifle_asset = Some(build_rifle(gpu, renderer));
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
        self.soldier_asset = Some(soldier);
        Ok(())
    }

    fn frame(&mut self, _dt: f32, input: &Input) {
        let delta = input.mouse_delta();
        self.view_yaw -= delta.x * openstrike_core::sim::MOUSE_SENS;
        self.view_pitch = (self.view_pitch - delta.y * openstrike_core::sim::MOUSE_SENS)
            .clamp(-89f32.to_radians(), 89f32.to_radians());
    }

    fn tick(&mut self, dt: f32, input: &Input) {
        self.tick_visual_effects(dt);
        self.update_network(dt, input);
        self.remote
            .advance_animation(dt, self.idle_clip, self.walk_clip, self.run_clip);
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

fn viewmodel_transform(eye: Vec3, yaw: f32, pitch: f32, recoil: f32) -> Mat4 {
    Mat4::from_translation(eye)
        * Mat4::from_rotation_y(yaw)
        * Mat4::from_rotation_x(pitch)
        * Mat4::from_translation(Vec3::new(7.2, -7.0, -8.5 + recoil * 2.8))
        * Mat4::from_rotation_x(recoil * 0.10)
        * Mat4::from_rotation_y(-0.03)
}

fn remote_rifle_transform(position: Vec3, yaw: f32, pitch: f32) -> Mat4 {
    Mat4::from_translation(position + Vec3::new(4.0, 19.0, -3.0))
        * Mat4::from_rotation_y(yaw)
        * Mat4::from_rotation_x(pitch.clamp(-0.65, 0.65))
        * Mat4::from_rotation_y(-0.03)
}

fn view_direction(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(-sy * cp, sp, -cy * cp)
}

fn development_map() -> MapData {
    use pocket3d::bsp::{
        Batch, DecodedTexture, MapGeometry, SpawnPoint, SunLight, SurfaceKind, WorldVertexData,
        lightmap::PAGE_SIZE, mesh::GeometryStats,
    };

    fn texture(name: &str, a: [u8; 4], b: [u8; 4]) -> DecodedTexture {
        DecodedTexture {
            name: name.into(),
            width: 2,
            height: 2,
            rgba: [a, b, b, a].into_iter().flatten().collect(),
            has_alpha: false,
        }
    }

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

    fn push_box(
        vertices: &mut Vec<WorldVertexData>,
        indices: &mut Vec<u32>,
        batches: &mut Vec<Batch>,
        texture: usize,
        min: Vec3,
        max: Vec3,
    ) {
        let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
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

    let floor_y = -36.0;
    let extent = 512.0;
    let wall_h = 130.0;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut batches = Vec::new();
    let floor_uv = [[0.0, 0.0], [0.0, 20.0], [20.0, 20.0], [20.0, 0.0]];
    push_quad(
        &mut vertices,
        &mut indices,
        &mut batches,
        0,
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
        push_quad(&mut vertices, &mut indices, &mut batches, 1, wall, wall_uv);
    }
    let pad = 74.0;
    let pad_uv = [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
    for (z, texture) in [(100.0, 2), (-100.0, 3)] {
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
    push_box(
        &mut vertices,
        &mut indices,
        &mut batches,
        4,
        Vec3::new(-18.0, floor_y, -22.0),
        Vec3::new(18.0, floor_y + 48.0, 22.0),
    );
    push_box(
        &mut vertices,
        &mut indices,
        &mut batches,
        4,
        Vec3::new(-300.0, floor_y, -210.0),
        Vec3::new(-220.0, floor_y + 55.0, -130.0),
    );
    push_box(
        &mut vertices,
        &mut indices,
        &mut batches,
        4,
        Vec3::new(220.0, floor_y, 130.0),
        Vec3::new(300.0, floor_y + 55.0, 210.0),
    );

    let textures = vec![
        texture("brushed-floor", [62, 66, 70, 255], [104, 112, 120, 255]),
        texture("concrete-wall", [96, 94, 88, 255], [132, 128, 118, 255]),
        texture("blue-spawn", [42, 88, 150, 255], [78, 134, 210, 255]),
        texture("red-spawn", [156, 56, 44, 255], [220, 94, 78, 255]),
        texture("cover-crate", [92, 72, 52, 255], [138, 105, 72, 255]),
    ];
    let mut lightmap = vec![230; (PAGE_SIZE * PAGE_SIZE * 4) as usize];
    for alpha in lightmap.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }

    MapData {
        name: "development-arena".into(),
        geometry: MapGeometry {
            vertices,
            indices,
            batches,
            lightmap_pages: vec![lightmap],
            stats: GeometryStats {
                faces_drawn: 1 + 4 + 2 + 5 * 3,
                faces_skipped: 0,
                triangles: 2 + 8 + 4 + 10 * 3,
            },
        },
        textures,
        entities: Vec::new(),
        collision: openstrike_fiber_arena::openstrike::empty_collision(),
        ct_spawns: vec![SpawnPoint {
            pos: Vec3::new(0.0, 0.0, 100.0),
            yaw: 0.0,
        }],
        t_spawns: vec![SpawnPoint {
            pos: Vec3::new(0.0, 0.0, -100.0),
            yaw: std::f32::consts::PI,
        }],
        sun: Some(SunLight {
            dir: Vec3::new(0.25, 0.72, 0.45).normalize(),
            color: Vec3::new(1.0, 0.95, 0.86),
        }),
        bounds: (
            Vec3::new(-extent, floor_y, -extent),
            Vec3::new(extent, 128.0, extent),
        ),
    }
}

fn build_rifle(gpu: &Gpu, renderer: &Renderer) -> Arc<ModelAsset> {
    use openstrike_core::weapon::{GUN_COLORS, rifle_boxes};
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for rifle_box in rifle_boxes() {
        add_box(
            &mut vertices,
            &mut indices,
            rifle_box.min,
            rifle_box.max,
            rifle_box.color,
        );
    }
    let pixels: Vec<_> = GUN_COLORS.into_iter().flatten().collect();
    ModelAsset::from_geometry(
        gpu,
        &renderer.model_material_layout,
        &renderer.samplers,
        "arena rifle",
        &vertices,
        &indices,
        Some((GUN_COLORS.len() as u32, 1, &pixels)),
    )
}

fn add_box(
    vertices: &mut Vec<ModelVertex>,
    indices: &mut Vec<u32>,
    min: Vec3,
    max: Vec3,
    color: usize,
) {
    let uv = [
        (color as f32 + 0.5) / openstrike_core::weapon::GUN_COLORS.len() as f32,
        0.5,
    ];
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
