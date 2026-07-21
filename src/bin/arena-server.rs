use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ed25519_dalek::SigningKey;
use openstrike_fiber_arena::{
    PROTOCOL_ID, TICK_HZ,
    net::{decode_player_identity, decode_player_name, mock_fiber_pubkey, unix_ms, unix_time},
    protocol::{
        ClientMessage, MatchPhase, PlayerBinding, PlayerSlot, ServerMessage, decode, encode,
    },
    security::load_secret_32,
    settlement::SettlementCoordinator,
    sim::{HarnessSim, MatchEvent, MatchSimulation},
};
use renet::{ClientId, ConnectionConfig, DefaultChannel, RenetServer, ServerEvent};
use renet_netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig};
use tracing::{info, warn};

#[cfg(feature = "openstrike")]
use openstrike_fiber_arena::openstrike::OpenStrikeDuelSim;

#[derive(Debug, Parser)]
#[command(about = "Authoritative OpenStrike Fiber Arena development server")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5000")]
    bind: SocketAddr,
    #[arg(long)]
    public_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 25)]
    damage_bucket: u16,
    #[arg(long, default_value_t = 1_000)]
    amount_per_bucket: u128,
    /// Total pre-authorized amount per player. This must be an exact multiple
    /// of --amount-per-bucket; the default creates four invoices per payer.
    #[arg(long, default_value_t = 4_000)]
    max_total_per_player: u128,
    /// Deadline for a released preimage to be processed by the payee client.
    #[arg(long, default_value_t = 30_000)]
    payment_deadline_ms: u64,
    #[arg(long, default_value_t = 7_200)]
    invoice_expiry_seconds: u64,
    #[arg(long, default_value_t = 3_600)]
    hold_payment_timeout_seconds: u64,
    /// FNN v0.9.0-rc7 production minimum is 160 minutes.
    #[arg(long, default_value_t = 9_600_000)]
    final_expiry_delta_ms: u64,
    /// 32-byte Netcode private key file shared only with the token issuer.
    #[arg(long, env = "ARENA_NETCODE_KEY_FILE", conflicts_with = "dev_unsecure")]
    netcode_key: Option<PathBuf>,
    /// Explicit local-development mode. It uses first-free seats and mock
    /// Fiber identities; funded play must use secure connect tokens.
    #[arg(long, default_value_t = false)]
    dev_unsecure: bool,
    /// 32-byte Ed25519 seed file used to sign settlement releases.
    #[arg(
        long,
        env = "ARENA_SIGNING_KEY_FILE",
        conflicts_with_all = ["signing_key_hex", "dev_signing_key"]
    )]
    signing_key_file: Option<PathBuf>,
    #[arg(
        long,
        env = "ARENA_SIGNING_KEY_HEX",
        conflicts_with_all = ["signing_key_file", "dev_signing_key"]
    )]
    signing_key_hex: Option<String>,
    #[arg(long, default_value_t = false)]
    dev_signing_key: bool,
    #[cfg(feature = "openstrike")]
    #[arg(long)]
    map: Option<PathBuf>,
    #[cfg(feature = "openstrike")]
    #[arg(long, default_value_t = false, conflicts_with = "map")]
    dev_arena: bool,
    #[cfg(feature = "openstrike")]
    #[arg(long = "wad-dir")]
    wad_dirs: Vec<PathBuf>,
}

fn main() -> Result<()> {
    openstrike_fiber_arena::net::init_tracing();
    let args = Args::parse();
    ensure!(args.damage_bucket > 0, "--damage-bucket must be positive");
    ensure!(
        args.amount_per_bucket > 0,
        "--amount-per-bucket must be positive"
    );
    ensure!(
        args.max_total_per_player >= args.amount_per_bucket
            && args.max_total_per_player % args.amount_per_bucket == 0,
        "--max-total-per-player must be a positive multiple of --amount-per-bucket"
    );
    ensure!(
        (args.max_total_per_player / args.amount_per_bucket).saturating_mul(2) <= u16::MAX as u128,
        "configured match cap creates too many hold invoices"
    );
    ensure!(
        args.payment_deadline_ms > 0,
        "--payment-deadline-ms must be positive"
    );
    ensure!(
        args.invoice_expiry_seconds > 0,
        "--invoice-expiry-seconds must be positive"
    );
    ensure!(
        args.hold_payment_timeout_seconds > 0,
        "--hold-payment-timeout-seconds must be positive"
    );
    ensure!(
        (9_600_000..=1_209_600_000).contains(&args.final_expiry_delta_ms),
        "FNN v0.9.0-rc7 requires --final-expiry-delta-ms between 9600000 and 1209600000"
    );

    let public_addr = args.public_addr.unwrap_or(args.bind);
    let socket = UdpSocket::bind(args.bind)
        .with_context(|| format!("binding authoritative server to {}", args.bind))?;
    let server_config = ServerConfig {
        current_time: unix_time(),
        max_clients: 2,
        protocol_id: PROTOCOL_ID,
        public_addresses: vec![public_addr],
        authentication: server_authentication(&args)?,
    };
    let mut transport = NetcodeServerTransport::new(server_config, socket)?;
    let mut network = RenetServer::new(ConnectionConfig::default());

    let match_id = unix_time().as_nanos();
    let signing_key = signing_key(&args)?;
    info!(
        signing_key_id = %hex::encode(&signing_key.verifying_key().to_bytes()[..8]),
        "loaded settlement signing key"
    );
    let mut settlement: Option<SettlementCoordinator> = None;
    let mut sim = build_simulation(&args)?;
    let mut clients = HashMap::<ClientId, PlayerSlot>::new();
    let mut players: [Option<PlayerBinding>; 2] = std::array::from_fn(|_| None);
    let mut accepted = HashSet::<PlayerSlot>::new();
    let initial_snapshot = sim.world_snapshot(match_id, 0, 0);
    let mut latest_inputs =
        initial_snapshot
            .players
            .map(|player| openstrike_fiber_arena::protocol::InputFrame {
                yaw: player.yaw,
                pitch: player.pitch,
                ..Default::default()
            });
    let mut started = false;
    let mut ended = false;
    let mut server_tick = 0u64;
    let fixed_step = Duration::from_secs_f64(1.0 / TICK_HZ as f64);
    let mut accumulator = Duration::ZERO;
    let mut last_update = Instant::now();

    info!(%match_id, bind = %args.bind, fnn = "0.9.0-rc7", "arena server ready");
    loop {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_update);
        last_update = now;
        accumulator += elapsed.min(Duration::from_millis(100));

        network.update(elapsed);
        transport.update(elapsed, &mut network)?;
        handle_connections(
            &mut network,
            &transport,
            &mut clients,
            &mut players,
            &mut accepted,
            args.dev_unsecure,
            settlement.is_some(),
        );

        if settlement.is_none() && clients.len() == 2 {
            let bound_players = std::array::from_fn(|index| {
                players[index]
                    .clone()
                    .expect("both connected slots have authenticated bindings")
            });
            let coordinator = SettlementCoordinator::new(
                match_id,
                signing_key.clone(),
                bound_players,
                args.amount_per_bucket,
                args.damage_bucket,
                args.max_total_per_player,
                args.payment_deadline_ms,
                args.invoice_expiry_seconds,
                args.hold_payment_timeout_seconds,
                args.final_expiry_delta_ms,
            );
            for (&client_id, &slot) in &clients {
                network.send_message(
                    client_id,
                    DefaultChannel::ReliableOrdered,
                    encode(&ServerMessage::Welcome {
                        slot,
                        terms: coordinator.terms().clone(),
                    })?,
                );
            }
            info!(%match_id, "both authenticated seats bound; preparing hold invoices");
            settlement = Some(coordinator);
        }

        if let Some(coordinator) = settlement.as_mut() {
            handle_client_messages(
                &mut network,
                &clients,
                &mut accepted,
                &mut latest_inputs,
                coordinator,
            );
        }

        if !started
            && !ended
            && clients.len() == 2
            && accepted.len() == 2
            && settlement
                .as_ref()
                .is_some_and(SettlementCoordinator::all_held)
        {
            started = true;
            sim.set_phase(MatchPhase::Live);
            broadcast_reliable(&mut network, &ServerMessage::MatchStarted { match_id })?;
            info!(%match_id, "all hold invoices received; match started");
        }

        while accumulator >= fixed_step {
            accumulator -= fixed_step;
            server_tick += 1;
            if started && !ended {
                let events = sim.tick(latest_inputs);
                for event in events {
                    match event {
                        MatchEvent::Damage {
                            attacker,
                            victim,
                            amount,
                            victim_health,
                        } => {
                            info!(
                                ?attacker,
                                ?victim,
                                amount,
                                victim_health,
                                "authoritative damage"
                            );
                            let coordinator = settlement
                                .as_mut()
                                .expect("a live match has a settlement coordinator");
                            match coordinator.record_damage(
                                attacker,
                                victim,
                                amount,
                                server_tick,
                                sim.state_hash(),
                                unix_ms(),
                            ) {
                                Ok(releases) => {
                                    for release in releases {
                                        let payee = release.intent.body.payee;
                                        let reservation_id = release.intent.body.reservation_id;
                                        info!(
                                            sequence = release.intent.body.sequence,
                                            reservation_id,
                                            ?payee,
                                            amount = %release.intent.body.amount,
                                            "releasing Fiber hold-invoice preimage"
                                        );
                                        if let Err(error) = send_reliable_to_slot(
                                            &mut network,
                                            &clients,
                                            payee,
                                            &ServerMessage::HoldInvoiceRelease(release),
                                        ) {
                                            warn!(
                                                %error,
                                                ?payee,
                                                reservation_id,
                                                "could not deliver Fiber hold-invoice preimage"
                                            );
                                        }
                                    }
                                }
                                Err(error) => {
                                    warn!(%error, ?victim, "held settlement capacity exhausted");
                                    sim.set_phase(MatchPhase::PaymentPaused { payer: victim });
                                }
                            }
                        }
                        MatchEvent::Death { killer, victim } => {
                            info!(?killer, ?victim, "match ended");
                            let coordinator = settlement
                                .as_mut()
                                .expect("a live match has a settlement coordinator");
                            for term in coordinator.cancel_unused() {
                                if let Err(error) = send_reliable_to_slot(
                                    &mut network,
                                    &clients,
                                    term.payee,
                                    &ServerMessage::CancelHoldInvoice {
                                        match_id,
                                        reservation_id: term.reservation_id,
                                        payment_hash: term.payment_hash,
                                    },
                                ) {
                                    warn!(
                                        %error,
                                        ?term.payee,
                                        reservation_id = term.reservation_id,
                                        "could not deliver Fiber hold-invoice cancellation"
                                    );
                                }
                            }
                            broadcast_reliable(
                                &mut network,
                                &ServerMessage::MatchEnded {
                                    match_id,
                                    winner: killer,
                                },
                            )?;
                            ended = true;
                        }
                    }
                }
                let latest_sequence = settlement
                    .as_ref()
                    .map_or(0, SettlementCoordinator::latest_sequence);
                let snapshot = sim.world_snapshot(match_id, server_tick, latest_sequence);
                network.broadcast_message(
                    DefaultChannel::Unreliable,
                    encode(&ServerMessage::Snapshot(snapshot))?,
                );
            }
        }

        transport.send_packets(&mut network);
        thread::sleep(Duration::from_millis(1));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_connections(
    network: &mut RenetServer,
    transport: &NetcodeServerTransport,
    clients: &mut HashMap<ClientId, PlayerSlot>,
    players: &mut [Option<PlayerBinding>; 2],
    accepted: &mut HashSet<PlayerSlot>,
    dev_unsecure: bool,
    terms_issued: bool,
) {
    while let Some(event) = network.get_event() {
        match event {
            ServerEvent::ClientConnected { client_id } => {
                if terms_issued {
                    warn!(%client_id, "rejecting late connection; reconnect recovery is not enabled");
                    network.disconnect(client_id);
                    continue;
                }
                let identity = if dev_unsecure {
                    let Some(slot) = PlayerSlot::ALL
                        .into_iter()
                        .find(|slot| !clients.values().any(|used| used == slot))
                    else {
                        network.disconnect(client_id);
                        continue;
                    };
                    let name = transport
                        .user_data(client_id)
                        .map(|data| decode_player_name(&data))
                        .unwrap_or_else(|| format!("player-{client_id}"));
                    Ok((
                        slot,
                        PlayerBinding {
                            name,
                            fiber_pubkey: mock_fiber_pubkey(slot).into(),
                        },
                    ))
                } else {
                    transport
                        .user_data(client_id)
                        .ok_or_else(|| "secure token omitted player identity".to_string())
                        .and_then(|data| decode_player_identity(&data))
                };
                let (slot, binding) = match identity {
                    Ok(identity) => identity,
                    Err(error) => {
                        warn!(%client_id, %error, "rejecting invalid connect-token identity");
                        network.disconnect(client_id);
                        continue;
                    }
                };
                if clients.values().any(|used| *used == slot) {
                    warn!(%client_id, ?slot, "authenticated seat is already occupied");
                    network.disconnect(client_id);
                    continue;
                }
                clients.insert(client_id, slot);
                players[slot.index()] = Some(binding.clone());
                info!(
                    %client_id,
                    ?slot,
                    name = %binding.name,
                    fiber_pubkey = %binding.fiber_pubkey,
                    "player identity bound"
                );
            }
            ServerEvent::ClientDisconnected { client_id, reason } => {
                let slot = clients.remove(&client_id);
                if let Some(slot) = slot {
                    accepted.remove(&slot);
                    if !terms_issued {
                        players[slot.index()] = None;
                    }
                }
                warn!(%client_id, ?slot, %reason, "player disconnected");
            }
        }
    }
}

fn handle_client_messages(
    network: &mut RenetServer,
    clients: &HashMap<ClientId, PlayerSlot>,
    accepted: &mut HashSet<PlayerSlot>,
    latest_inputs: &mut [openstrike_fiber_arena::protocol::InputFrame; 2],
    settlement: &mut SettlementCoordinator,
) {
    let client_ids: Vec<_> = network.clients_id().into_iter().collect();
    for client_id in client_ids {
        let Some(slot) = clients.get(&client_id).copied() else {
            continue;
        };
        while let Some(bytes) = network.receive_message(client_id, DefaultChannel::Unreliable) {
            if let Ok(ClientMessage::Input(input)) = decode(&bytes) {
                let input = input.sanitized();
                if input.sequence >= latest_inputs[slot.index()].sequence {
                    latest_inputs[slot.index()] = input;
                }
            }
        }
        while let Some(bytes) = network.receive_message(client_id, DefaultChannel::ReliableOrdered)
        {
            match decode::<ClientMessage>(&bytes) {
                Ok(ClientMessage::AcceptTerms { match_id })
                    if match_id == settlement.terms().match_id =>
                {
                    accepted.insert(slot);
                    info!(?slot, "player accepted bound match terms");
                }
                Ok(ClientMessage::HoldInvoiceOffer(offer)) => {
                    let forward = offer.clone();
                    match settlement.register_invoice_offer(slot, offer) {
                        Ok(payer) => {
                            if let Err(error) = send_reliable_to_slot(
                                network,
                                clients,
                                payer,
                                &ServerMessage::HoldInvoiceOffer(forward),
                            ) {
                                warn!(%error, ?payer, "could not forward hold invoice");
                            }
                        }
                        Err(error) => warn!(?slot, %error, "rejected hold-invoice offer"),
                    }
                }
                Ok(ClientMessage::HoldInvoiceAck(ack)) => {
                    let reservation_id = ack.reservation_id;
                    let stage = ack.stage;
                    match settlement.acknowledge(slot, ack) {
                        Ok(()) => info!(?slot, reservation_id, ?stage, "hold-invoice progress"),
                        Err(error) => {
                            warn!(?slot, reservation_id, ?stage, %error, "rejected hold-invoice acknowledgement")
                        }
                    }
                }
                Ok(ClientMessage::HoldInvoiceFailure(failure)) => {
                    let reservation_id = failure.reservation_id;
                    let stage = failure.stage;
                    let message = failure.error.clone();
                    match settlement.record_failure(slot, &failure) {
                        Ok(()) => {
                            warn!(?slot, reservation_id, ?stage, error = %message, "Fiber hold-invoice operation failed")
                        }
                        Err(error) => {
                            warn!(?slot, reservation_id, ?stage, %error, "rejected hold-invoice failure report")
                        }
                    }
                }
                Ok(ClientMessage::Input(_)) => {}
                Ok(_) => warn!(?slot, "ignored invalid control message"),
                Err(error) => warn!(?slot, %error, "failed to decode client message"),
            }
        }
    }
}

fn build_simulation(args: &Args) -> Result<Box<dyn MatchSimulation>> {
    #[cfg(feature = "openstrike")]
    if args.dev_arena {
        info!("using asset-free OpenStrike development arena");
        return Ok(Box::new(OpenStrikeDuelSim::dev_arena()));
    }

    #[cfg(feature = "openstrike")]
    if let Some(map) = &args.map {
        let (simulation, _) = OpenStrikeDuelSim::load(map, &args.wad_dirs)?;
        info!(map = %map.display(), "using OpenStrike BSP simulation");
        return Ok(Box::new(simulation));
    }

    #[cfg(not(feature = "openstrike"))]
    let _ = args;

    info!("using deterministic development harness");
    Ok(Box::new(HarnessSim::default()))
}

fn send_reliable_to_slot(
    network: &mut RenetServer,
    clients: &HashMap<ClientId, PlayerSlot>,
    slot: PlayerSlot,
    message: &ServerMessage,
) -> Result<()> {
    let client_id = clients
        .iter()
        .find_map(|(client_id, bound_slot)| (*bound_slot == slot).then_some(*client_id))
        .with_context(|| format!("no connected client for slot {slot:?}"))?;
    network.send_message(client_id, DefaultChannel::ReliableOrdered, encode(message)?);
    Ok(())
}

fn broadcast_reliable(network: &mut RenetServer, message: &ServerMessage) -> Result<()> {
    network.broadcast_message(DefaultChannel::ReliableOrdered, encode(message)?);
    Ok(())
}

fn signing_key(args: &Args) -> Result<SigningKey> {
    if let Some(path) = &args.signing_key_file {
        return Ok(SigningKey::from_bytes(&load_secret_32(
            path,
            "settlement signing key",
        )?));
    }
    if let Some(value) = &args.signing_key_hex {
        let bytes = hex::decode(value.trim_start_matches("0x"))
            .context("ARENA_SIGNING_KEY_HEX must be hex")?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("ARENA_SIGNING_KEY_HEX must contain 32 bytes"))?;
        return Ok(SigningKey::from_bytes(&bytes));
    }
    if args.dev_signing_key {
        warn!("using deterministic development signing key");
        return Ok(SigningKey::from_bytes(&[0x42; 32]));
    }
    anyhow::bail!(
        "settlement signing key required: pass --signing-key-file/--signing-key-hex, or explicitly use --dev-signing-key"
    )
}

fn server_authentication(args: &Args) -> Result<ServerAuthentication> {
    if args.dev_unsecure {
        warn!("using unauthenticated development transport");
        return Ok(ServerAuthentication::Unsecure);
    }
    let path = args.netcode_key.as_deref().context(
        "secure Netcode key required: pass --netcode-key, or explicitly use --dev-unsecure",
    )?;
    Ok(ServerAuthentication::Secure {
        private_key: load_secret_32(path, "Netcode private key")?,
    })
}
