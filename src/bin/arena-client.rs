use std::{
    collections::HashSet,
    f32::consts::PI,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use openstrike_fiber_arena::{
    TICK_HZ,
    client::SettlementGuard,
    fiber::{FiberRpcClient, mock_success_ack},
    net::{init_tracing, unix_ms, unix_time},
    protocol::{
        ClientMessage, InputFrame, MatchPhase, PaymentStatus, PlayerSlot, ServerMessage,
        SettlementAck, decode, encode,
    },
    security::client_authentication,
};
use renet::{ConnectionConfig, DefaultChannel, RenetClient};
use renet_netcode::NetcodeClientTransport;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(about = "Headless OpenStrike Fiber Arena development client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5000")]
    server: SocketAddr,
    #[arg(long)]
    name: String,
    /// Short-lived token issued by the matchmaking service or arena-admin.
    #[arg(long, conflicts_with = "dev_unsecure")]
    connect_token: Option<PathBuf>,
    /// Explicit opt-in for local development without Netcode authentication.
    #[arg(long, default_value_t = false)]
    dev_unsecure: bool,
    /// Fire continuously toward the opponent. Used by the headless smoke test.
    #[arg(long, default_value_t = false)]
    auto_fire: bool,
    #[arg(long, default_value_t = false, conflicts_with = "fiber_rpc")]
    mock_payments: bool,
    #[arg(long, env = "FIBER_RPC_URL")]
    fiber_rpc: Option<String>,
    #[arg(long, env = "FIBER_PEER_PUBKEY", requires = "fiber_rpc")]
    peer_pubkey: Option<String>,
    #[arg(long, default_value_t = false)]
    exit_on_end: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    if !args.mock_payments && args.fiber_rpc.is_none() {
        bail!("choose --mock-payments or configure --fiber-rpc and --peer-pubkey");
    }

    let socket = UdpSocket::bind("0.0.0.0:0").context("binding client UDP socket")?;
    let client_id = unix_time().as_nanos().min(u64::MAX as u128) as u64;
    let auth = client_authentication(
        args.connect_token.as_deref(),
        args.dev_unsecure,
        args.server,
        client_id,
        &args.name,
    )?;
    let mut transport = NetcodeClientTransport::new(unix_time(), auth, socket)?;
    let mut network = RenetClient::new(ConnectionConfig::default());
    let fiber = args.fiber_rpc.as_ref().map(FiberRpcClient::new);
    let (payment_tx, mut payment_rx) = tokio::sync::mpsc::unbounded_channel::<SettlementAck>();

    let mut slot = None;
    let mut settlement_guard: Option<SettlementGuard> = None;
    let mut running = false;
    let mut input_sequence = 0u32;
    let mut client_tick = 0u64;
    let mut pending_payments = HashSet::new();
    let mut last_health = [100u16; 2];
    let mut last_update = Instant::now();
    let fixed_step = Duration::from_secs_f64(1.0 / TICK_HZ as f64);
    let mut accumulator = Duration::ZERO;
    let mut match_end_received = false;
    let mut exit_after = None;

    info!(name = %args.name, server = %args.server, "connecting");
    loop {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_update);
        last_update = now;
        accumulator += elapsed.min(Duration::from_millis(100));

        network.update(elapsed);
        transport.update(elapsed, &mut network)?;

        while let Some(bytes) = network.receive_message(DefaultChannel::ReliableOrdered) {
            match decode::<ServerMessage>(&bytes) {
                Ok(ServerMessage::Welcome {
                    slot: assigned,
                    terms: assigned_terms,
                }) => {
                    info!(?assigned, match_id = %assigned_terms.match_id, "joined match");
                    slot = Some(assigned);
                    network.send_message(
                        DefaultChannel::ReliableOrdered,
                        encode(&ClientMessage::AcceptTerms {
                            match_id: assigned_terms.match_id,
                        })?,
                    );
                    settlement_guard = Some(SettlementGuard::new(assigned_terms));
                }
                Ok(ServerMessage::MatchStarted { match_id }) => {
                    info!(%match_id, "match started");
                    running = true;
                }
                Ok(ServerMessage::SettlementIntent(intent)) => {
                    let Some(local_slot) = slot else {
                        continue;
                    };
                    let Some(guard) = settlement_guard.as_mut() else {
                        continue;
                    };
                    if intent.body.payer != local_slot {
                        info!(
                            sequence = intent.body.sequence,
                            ?intent.body.payer,
                            amount = %intent.body.amount,
                            "opponent payment requested"
                        );
                        continue;
                    }
                    let payment_deadline_ms = guard.terms().payment_deadline_ms;
                    let validation = guard.validate(&intent, unix_ms());
                    if let Err(error_message) = validation {
                        warn!(
                            sequence = intent.body.sequence,
                            error = %error_message,
                            "refusing settlement intent"
                        );
                        network.send_message(
                            DefaultChannel::ReliableOrdered,
                            encode(&ClientMessage::SettlementAck(SettlementAck {
                                match_id: intent.body.match_id,
                                settlement_sequence: intent.body.sequence,
                                payment_hash: None,
                                status: PaymentStatus::Failed {
                                    error: error_message,
                                },
                            }))?,
                        );
                        continue;
                    }

                    info!(
                        sequence = intent.body.sequence,
                        amount = %intent.body.amount,
                        "executing Fiber settlement intent"
                    );
                    pending_payments.insert(intent.body.sequence);
                    if args.mock_payments {
                        payment_tx.send(mock_success_ack(&intent)).ok();
                    } else {
                        let rpc = fiber.clone().expect("validated Fiber configuration");
                        let target = args.peer_pubkey.clone().expect("validated peer public key");
                        let tx = payment_tx.clone();
                        let timeout = Duration::from_millis(payment_deadline_ms);
                        tokio::spawn(async move {
                            let ack = match rpc.execute_intent(&intent, &target, timeout).await {
                                Ok(ack) => ack,
                                Err(error) => SettlementAck {
                                    match_id: intent.body.match_id,
                                    settlement_sequence: intent.body.sequence,
                                    payment_hash: None,
                                    status: PaymentStatus::Failed {
                                        error: error.to_string(),
                                    },
                                },
                            };
                            tx.send(ack).ok();
                        });
                    }
                }
                Ok(ServerMessage::MatchEnded { match_id, winner }) => {
                    info!(%match_id, ?winner, "match ended");
                    running = false;
                    match_end_received = true;
                }
                Ok(ServerMessage::Error(message)) => error!(%message, "server error"),
                Ok(ServerMessage::Snapshot(_)) => {}
                Err(error) => warn!(%error, "failed to decode server control message"),
            }
        }

        while let Some(bytes) = network.receive_message(DefaultChannel::Unreliable) {
            if let Ok(ServerMessage::Snapshot(snapshot)) = decode(&bytes) {
                for player in snapshot.players {
                    if last_health[player.slot.index()] != player.health {
                        info!(
                            ?player.slot,
                            health = player.health,
                            tick = snapshot.server_tick,
                            "health changed"
                        );
                        last_health[player.slot.index()] = player.health;
                    }
                }
                if matches!(snapshot.phase, MatchPhase::PaymentPaused { .. }) {
                    warn!(?snapshot.phase, "match paused for payment");
                }
            }
        }

        while let Ok(ack) = payment_rx.try_recv() {
            let settlement_sequence = ack.settlement_sequence;
            info!(
                sequence = settlement_sequence,
                ?ack.status,
                "payment worker completed"
            );
            network.send_message(
                DefaultChannel::ReliableOrdered,
                encode(&ClientMessage::SettlementAck(ack))?,
            );
            pending_payments.remove(&settlement_sequence);
        }

        if args.exit_on_end
            && match_end_received
            && pending_payments.is_empty()
            && exit_after.is_none()
        {
            // Give Renet a short window to deliver the last reliable payment
            // acknowledgement before the development client exits.
            exit_after = Some(Instant::now() + Duration::from_millis(250));
        }

        while accumulator >= fixed_step {
            accumulator -= fixed_step;
            client_tick += 1;
            if running
                && network.is_connected()
                && let Some(local_slot) = slot
            {
                input_sequence = input_sequence.wrapping_add(1);
                let input = InputFrame {
                    sequence: input_sequence,
                    client_tick,
                    yaw: match local_slot {
                        PlayerSlot::A => 0.0,
                        PlayerSlot::B => PI,
                    },
                    fire: args.auto_fire,
                    ..Default::default()
                };
                network.send_message(
                    DefaultChannel::Unreliable,
                    encode(&ClientMessage::Input(input))?,
                );
            }
        }

        if let Some(reason) = network.disconnect_reason() {
            bail!("disconnected from server: {reason}");
        }
        transport.send_packets(&mut network)?;
        if exit_after.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
