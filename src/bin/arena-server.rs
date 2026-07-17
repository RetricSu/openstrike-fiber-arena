use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Parser;
use ed25519_dalek::SigningKey;
use openstrike_fiber_arena::{
    PROTOCOL_ID, TICK_HZ,
    net::{decode_player_name, init_tracing, unix_ms, unix_time},
    protocol::{ClientMessage, MatchPhase, PlayerSlot, ServerMessage, decode, encode},
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
    #[arg(long, default_value_t = 100_000)]
    max_total_per_player: u128,
    #[arg(long, default_value_t = 2_000)]
    payment_deadline_ms: u64,
    /// 32-byte Netcode private key file shared only with the token issuer.
    #[arg(long, env = "ARENA_NETCODE_KEY_FILE", conflicts_with = "dev_unsecure")]
    netcode_key: Option<PathBuf>,
    /// Explicit local-development mode. Production defaults to secure tokens.
    #[arg(long, default_value_t = false)]
    dev_unsecure: bool,
    /// 32-byte Ed25519 seed file used to sign settlement intents.
    #[arg(
        long,
        env = "ARENA_SIGNING_KEY_FILE",
        conflicts_with_all = ["signing_key_hex", "dev_signing_key"]
    )]
    signing_key_file: Option<PathBuf>,
    /// 32-byte Ed25519 signing seed in hex. A deterministic development key
    /// is never selected implicitly.
    #[arg(
        long,
        env = "ARENA_SIGNING_KEY_HEX",
        conflicts_with_all = ["signing_key_file", "dev_signing_key"]
    )]
    signing_key_hex: Option<String>,
    #[arg(long, default_value_t = false)]
    dev_signing_key: bool,
    /// GoldSrc BSP map used by the OpenStrike simulation. Requires the
    /// `openstrike` Cargo feature; omit it to use the deterministic harness.
    #[cfg(feature = "openstrike")]
    #[arg(long)]
    map: Option<PathBuf>,
    /// Run an asset-free OpenStrike arena for desktop/network smoke tests.
    #[cfg(feature = "openstrike")]
    #[arg(long, default_value_t = false, conflicts_with = "map")]
    dev_arena: bool,
    /// Directory containing WAD files referenced by the BSP map.
    #[cfg(feature = "openstrike")]
    #[arg(long = "wad-dir")]
    wad_dirs: Vec<PathBuf>,
}

fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
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
    let mut settlement = SettlementCoordinator::new(
        match_id,
        signing_key,
        args.amount_per_bucket,
        args.damage_bucket,
        args.max_total_per_player,
        args.payment_deadline_ms,
    );
    let mut sim = build_simulation(&args)?;
    let mut clients = HashMap::<ClientId, PlayerSlot>::new();
    let mut names = HashMap::<ClientId, String>::new();
    let mut accepted = HashSet::<PlayerSlot>::new();
    let initial_snapshot = sim.world_snapshot(match_id, 0, settlement.latest_sequence());
    let mut latest_inputs =
        initial_snapshot
            .players
            .map(|player| openstrike_fiber_arena::protocol::InputFrame {
                yaw: player.yaw,
                pitch: player.pitch,
                ..Default::default()
            });
    let mut started = false;
    let mut server_tick = 0u64;
    let fixed_step = Duration::from_secs_f64(1.0 / TICK_HZ as f64);
    let mut accumulator = Duration::ZERO;
    let mut last_update = Instant::now();

    info!(%match_id, bind = %args.bind, "arena server ready");
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
            &mut names,
            &mut accepted,
            settlement.terms(),
        )?;
        handle_client_messages(
            &mut network,
            &clients,
            &mut accepted,
            &mut latest_inputs,
            &mut settlement,
        );

        if !started && clients.len() == 2 && accepted.len() == 2 {
            started = true;
            sim.set_phase(MatchPhase::Live);
            broadcast_reliable(&mut network, &ServerMessage::MatchStarted { match_id })?;
            info!(%match_id, "both players accepted terms; match started");
        }

        while accumulator >= fixed_step {
            accumulator -= fixed_step;
            server_tick += 1;
            if started {
                update_payment_gate(sim.as_mut(), &settlement);
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
                            match settlement.record_damage(
                                attacker,
                                victim,
                                amount,
                                server_tick,
                                sim.state_hash(),
                                unix_ms(),
                            ) {
                                Ok(intents) => {
                                    for intent in intents {
                                        info!(
                                            sequence = intent.body.sequence,
                                            ?intent.body.payer,
                                            amount = %intent.body.amount,
                                            "issuing Fiber settlement intent"
                                        );
                                        broadcast_reliable(
                                            &mut network,
                                            &ServerMessage::SettlementIntent(intent),
                                        )?;
                                    }
                                }
                                Err(error) => {
                                    warn!(%error, ?victim, "could not issue settlement intent");
                                    sim.set_phase(MatchPhase::PaymentPaused { payer: victim });
                                }
                            }
                        }
                        MatchEvent::Death { killer, victim } => {
                            info!(?killer, ?victim, "match ended");
                            broadcast_reliable(
                                &mut network,
                                &ServerMessage::MatchEnded {
                                    match_id,
                                    winner: killer,
                                },
                            )?;
                        }
                    }
                }
                let snapshot =
                    sim.world_snapshot(match_id, server_tick, settlement.latest_sequence());
                let message = encode(&ServerMessage::Snapshot(snapshot))?;
                network.broadcast_message(DefaultChannel::Unreliable, message);
            }
        }

        transport.send_packets(&mut network);
        thread::sleep(Duration::from_millis(1));
    }
}

fn handle_connections(
    network: &mut RenetServer,
    transport: &NetcodeServerTransport,
    clients: &mut HashMap<ClientId, PlayerSlot>,
    names: &mut HashMap<ClientId, String>,
    accepted: &mut HashSet<PlayerSlot>,
    terms: &openstrike_fiber_arena::protocol::MatchTerms,
) -> Result<()> {
    while let Some(event) = network.get_event() {
        match event {
            ServerEvent::ClientConnected { client_id } => {
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
                clients.insert(client_id, slot);
                names.insert(client_id, name.clone());
                let message = ServerMessage::Welcome {
                    slot,
                    terms: terms.clone(),
                };
                network.send_message(
                    client_id,
                    DefaultChannel::ReliableOrdered,
                    encode(&message)?,
                );
                info!(%client_id, ?slot, %name, "player connected");
            }
            ServerEvent::ClientDisconnected { client_id, reason } => {
                let slot = clients.remove(&client_id);
                if let Some(slot) = slot {
                    accepted.remove(&slot);
                }
                let name = names.remove(&client_id);
                warn!(%client_id, ?slot, ?name, %reason, "player disconnected");
            }
        }
    }
    Ok(())
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
                    if input.sequence == 1 {
                        info!(
                            ?slot,
                            yaw = input.yaw,
                            pitch = input.pitch,
                            fire = input.fire,
                            "received first player input"
                        );
                    }
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
                    info!(?slot, "player accepted match terms");
                }
                Ok(ClientMessage::SettlementAck(ack)) => {
                    let sequence = ack.settlement_sequence;
                    let status = ack.status.clone();
                    if let Err(error) = settlement.acknowledge(slot, ack) {
                        warn!(?slot, %error, sequence, "rejected settlement acknowledgement");
                    } else {
                        info!(?slot, ?status, sequence, "settlement acknowledgement");
                    }
                }
                Ok(ClientMessage::Input(_)) => {}
                Ok(_) => warn!(?slot, "ignored invalid control message"),
                Err(error) => warn!(?slot, %error, "failed to decode client message"),
            }
        }
    }
}

fn update_payment_gate(sim: &mut dyn MatchSimulation, settlement: &SettlementCoordinator) {
    match (sim.phase(), settlement.blocking_payer(unix_ms())) {
        (MatchPhase::Live, Some(payer)) => {
            warn!(?payer, "payment credit window exceeded; pausing match");
            sim.set_phase(MatchPhase::PaymentPaused { payer });
        }
        (MatchPhase::PaymentPaused { .. }, None) => {
            info!("all blocking payments cleared; resuming match");
            sim.set_phase(MatchPhase::Live);
        }
        _ => {}
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
