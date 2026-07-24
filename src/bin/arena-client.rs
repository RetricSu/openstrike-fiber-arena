use std::{
    f32::consts::PI,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use openstrike_fiber_arena::{
    TICK_HZ,
    client::SettlementGuard,
    fiber::{FiberCurrency, FiberRpcClient, HoldInvoiceExpectation},
    matchmaking::{EnterRoomRequest, MatchmakerClient},
    net::{init_tracing, unix_ms, unix_time},
    protocol::{
        ClientMessage, HoldInvoiceAck, HoldInvoiceFailure, HoldInvoiceOffer, HoldInvoiceStage,
        HoldInvoiceTerm, InputFrame, MatchPhase, MatchTerms, PlayerSlot, ServerMessage, decode,
        encode,
    },
    security::{client_authentication, client_authentication_with_token},
};
use renet::{ConnectionConfig, DefaultChannel, RenetClient};
use renet_netcode::{ConnectToken, NetcodeClientTransport};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(about = "Headless OpenStrike Fiber Arena development client")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5000")]
    server: SocketAddr,
    #[arg(long)]
    name: String,
    #[arg(long, conflicts_with = "dev_unsecure")]
    connect_token: Option<PathBuf>,
    /// HTTP room service. Omit --room to create a room and print its code.
    #[arg(long, conflicts_with_all = ["connect_token", "dev_unsecure"])]
    matchmaker: Option<String>,
    #[arg(long, requires = "matchmaker")]
    room: Option<String>,
    #[arg(long, default_value_t = 300)]
    matchmaking_timeout_seconds: u64,
    #[arg(long, default_value_t = false)]
    dev_unsecure: bool,
    #[arg(long, default_value_t = false)]
    auto_fire: bool,
    #[arg(long, default_value_t = false, conflicts_with = "fiber_rpc")]
    mock_payments: bool,
    #[arg(long, env = "FIBER_RPC_URL")]
    fiber_rpc: Option<String>,
    #[arg(long, env = "FIBER_CURRENCY", default_value = "Fibt")]
    fiber_currency: FiberCurrency,
    #[arg(long, default_value_t = false)]
    exit_on_end: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
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
    let (server, matchmaker_token) = resolve_connection(&args).await?;

    let socket = UdpSocket::bind("0.0.0.0:0").context("binding client UDP socket")?;
    let client_id = unix_time().as_nanos().min(u64::MAX as u128) as u64;
    let auth = match matchmaker_token {
        Some(token) => {
            client_authentication_with_token(Some(token), false, server, client_id, &args.name)?
        }
        None => client_authentication(
            args.connect_token.as_deref(),
            args.dev_unsecure,
            server,
            client_id,
            &args.name,
        )?,
    };
    let mut transport = NetcodeClientTransport::new(unix_time(), auth, socket)?;
    let mut network = RenetClient::new(ConnectionConfig::default());
    let fiber = args.fiber_rpc.as_ref().map(FiberRpcClient::new);
    let (fiber_tx, mut fiber_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    let mut slot = None;
    let mut settlement_guard: Option<SettlementGuard> = None;
    let mut running = false;
    let mut input_sequence = 0u32;
    let mut client_tick = 0u64;
    let mut pending_operations = 0usize;
    let mut last_health = [100u16; 2];
    let mut last_update = Instant::now();
    let fixed_step = Duration::from_secs_f64(1.0 / TICK_HZ as f64);
    let mut accumulator = Duration::ZERO;
    let mut match_end_received = false;
    let mut exit_after = None;

    info!(name = %args.name, %server, "connecting");
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
                    terms,
                }) => {
                    prepare_hold_invoices(
                        &args,
                        &mut network,
                        fiber.as_ref(),
                        &fiber_tx,
                        assigned,
                        &terms,
                        &mut pending_operations,
                    )
                    .await?;
                    info!(
                        ?assigned,
                        match_id = %terms.match_id,
                        fiber_pubkey = %terms.players[assigned.index()].fiber_pubkey,
                        "joined bound match; preparing holds"
                    );
                    slot = Some(assigned);
                    network.send_message(
                        DefaultChannel::ReliableOrdered,
                        encode(&ClientMessage::AcceptTerms {
                            match_id: terms.match_id,
                        })?,
                    );
                    settlement_guard = Some(SettlementGuard::new(terms));
                }
                Ok(ServerMessage::HoldInvoiceOffer(offer)) => {
                    let Some(local_slot) = slot else {
                        continue;
                    };
                    let Some(guard) = settlement_guard.as_ref() else {
                        continue;
                    };
                    let term = checked_term(guard.terms(), &offer)?;
                    ensure!(
                        term.payer == local_slot,
                        "invoice offer sent to wrong payer"
                    );
                    if args.mock_payments {
                        send_control(
                            &mut network,
                            ClientMessage::HoldInvoiceAck(ack_for(
                                guard.terms().match_id,
                                term,
                                HoldInvoiceStage::Funded,
                            )),
                        )?;
                    } else {
                        pending_operations += 1;
                        spawn_fund_invoice(
                            fiber.clone().expect("validated Fiber configuration"),
                            args.fiber_currency,
                            guard.terms().clone(),
                            term.clone(),
                            offer.invoice,
                            fiber_tx.clone(),
                        );
                    }
                }
                Ok(ServerMessage::MatchStarted { match_id }) => {
                    info!(%match_id, "match started after all invoices reached Received");
                    running = true;
                }
                Ok(ServerMessage::HoldInvoiceRelease(release)) => {
                    let Some(local_slot) = slot else {
                        continue;
                    };
                    let Some(guard) = settlement_guard.as_mut() else {
                        continue;
                    };
                    if let Err(message) = guard.validate_release(&release, local_slot, unix_ms()) {
                        warn!(
                            reservation_id = release.intent.body.reservation_id,
                            error = %message,
                            "refusing invalid hold-invoice release"
                        );
                        send_control(
                            &mut network,
                            ClientMessage::HoldInvoiceFailure(failure_for(
                                release.intent.body.match_id,
                                release.intent.body.reservation_id,
                                release.intent.body.payment_hash,
                                HoldInvoiceStage::Settled,
                                message,
                            )),
                        )?;
                        continue;
                    }
                    let term = guard
                        .terms()
                        .hold_invoices
                        .iter()
                        .find(|term| term.reservation_id == release.intent.body.reservation_id)
                        .expect("validated reservation")
                        .clone();
                    if args.mock_payments {
                        send_control(
                            &mut network,
                            ClientMessage::HoldInvoiceAck(ack_for(
                                guard.terms().match_id,
                                &term,
                                HoldInvoiceStage::Settled,
                            )),
                        )?;
                    } else {
                        pending_operations += 1;
                        spawn_settle_invoice(
                            fiber.clone().expect("validated Fiber configuration"),
                            guard.terms().clone(),
                            term,
                            release.payment_preimage,
                            fiber_tx.clone(),
                        );
                    }
                }
                Ok(ServerMessage::CancelHoldInvoice {
                    match_id,
                    reservation_id,
                    payment_hash,
                }) => {
                    let Some(local_slot) = slot else {
                        continue;
                    };
                    let Some(guard) = settlement_guard.as_ref() else {
                        continue;
                    };
                    let term = guard
                        .terms()
                        .hold_invoices
                        .iter()
                        .find(|term| term.reservation_id == reservation_id)
                        .context("server requested cancellation of unknown reservation")?
                        .clone();
                    ensure!(
                        match_id == guard.terms().match_id,
                        "wrong cancellation match"
                    );
                    ensure!(term.payee == local_slot, "cancellation sent to wrong payee");
                    ensure!(term.payment_hash == payment_hash, "wrong cancellation hash");
                    if args.mock_payments {
                        send_control(
                            &mut network,
                            ClientMessage::HoldInvoiceAck(ack_for(
                                match_id,
                                &term,
                                HoldInvoiceStage::Cancelled,
                            )),
                        )?;
                    } else {
                        pending_operations += 1;
                        spawn_cancel_invoice(
                            fiber.clone().expect("validated Fiber configuration"),
                            match_id,
                            term,
                            fiber_tx.clone(),
                        );
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
                    warn!(?snapshot.phase, "match paused: prepaid capacity exhausted");
                }
            }
        }

        while let Ok(message) = fiber_rx.try_recv() {
            pending_operations = pending_operations.saturating_sub(1);
            match &message {
                ClientMessage::HoldInvoiceAck(ack) => info!(
                    reservation_id = ack.reservation_id,
                    ?ack.stage,
                    "Fiber hold-invoice worker completed"
                ),
                ClientMessage::HoldInvoiceFailure(failure) => warn!(
                    reservation_id = failure.reservation_id,
                    ?failure.stage,
                    error = %failure.error,
                    "Fiber hold-invoice worker failed"
                ),
                _ => {}
            }
            send_control(&mut network, message)?;
        }

        if args.exit_on_end && match_end_received && pending_operations == 0 && exit_after.is_none()
        {
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
                send_control(
                    &mut network,
                    ClientMessage::Input(InputFrame {
                        sequence: input_sequence,
                        client_tick,
                        yaw: match local_slot {
                            PlayerSlot::A => 0.0,
                            PlayerSlot::B => PI,
                        },
                        fire: args.auto_fire,
                        ..Default::default()
                    }),
                )?;
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

async fn resolve_connection(args: &Args) -> Result<(SocketAddr, Option<ConnectToken>)> {
    let Some(matchmaker_url) = &args.matchmaker else {
        return Ok((args.server, None));
    };
    let fiber_rpc = args
        .fiber_rpc
        .as_deref()
        .context("--matchmaker requires --fiber-rpc or FIBER_RPC_URL")?;
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
    info!(
        room = %initial.room_code,
        slot = ?initial.slot,
        "waiting for opponent"
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
    let (server, token) = ready.ready_connection()?;
    info!(room = %ready.room_code, %server, "room ready");
    Ok((server, Some(token)))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hold_invoices(
    args: &Args,
    network: &mut RenetClient,
    fiber: Option<&FiberRpcClient>,
    fiber_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    slot: PlayerSlot,
    terms: &MatchTerms,
    pending_operations: &mut usize,
) -> Result<()> {
    let binding = &terms.players[slot.index()];
    ensure!(
        binding.name == args.name,
        "connect token is bound to player {}, not {}",
        binding.name,
        args.name
    );

    if let Some(rpc) = fiber {
        let readiness = rpc
            .check_direct_channel(
                &terms.players[slot.opponent().index()].fiber_pubkey,
                terms.max_total_per_player,
            )
            .await
            .context("FNN v0.9.0-rc7 direct-channel preflight")?;
        ensure!(
            readiness.node.pubkey == binding.fiber_pubkey,
            "local FNN pubkey {} does not match connect-token binding {}",
            readiness.node.pubkey,
            binding.fiber_pubkey
        );
        info!(
            fnn_version = %readiness.node.version,
            channel_id = %readiness.channel.channel_id,
            outbound = %readiness.channel.local_balance,
            "bound Fiber direct channel ready"
        );
    }

    for term in terms.hold_invoices.iter().filter(|term| term.payee == slot) {
        let invoice = if let Some(rpc) = fiber {
            let expectation = HoldInvoiceExpectation::new(
                terms,
                term,
                &binding.fiber_pubkey,
                args.fiber_currency,
            )?;
            rpc.create_hold_invoice(&expectation)
                .await
                .with_context(|| format!("creating hold invoice {}", term.reservation_id))?
        } else {
            format!(
                "mock:{}:{}:{}",
                terms.match_id,
                term.reservation_id,
                hex::encode(term.payment_hash)
            )
        };
        send_control(
            network,
            ClientMessage::HoldInvoiceOffer(HoldInvoiceOffer {
                match_id: terms.match_id,
                reservation_id: term.reservation_id,
                payment_hash: term.payment_hash,
                invoice,
            }),
        )?;

        if let Some(rpc) = fiber.cloned() {
            *pending_operations += 1;
            let tx = fiber_tx.clone();
            let term = term.clone();
            let match_id = terms.match_id;
            let timeout = Duration::from_millis(terms.payment_deadline_ms);
            tokio::spawn(async move {
                let message = match rpc.wait_invoice_received(term.payment_hash, timeout).await {
                    Ok(()) => ClientMessage::HoldInvoiceAck(ack_for(
                        match_id,
                        &term,
                        HoldInvoiceStage::Received,
                    )),
                    Err(error) => ClientMessage::HoldInvoiceFailure(failure_for(
                        match_id,
                        term.reservation_id,
                        term.payment_hash,
                        HoldInvoiceStage::Received,
                        error.to_string(),
                    )),
                };
                tx.send(message).ok();
            });
        } else {
            send_control(
                network,
                ClientMessage::HoldInvoiceAck(ack_for(
                    terms.match_id,
                    term,
                    HoldInvoiceStage::Received,
                )),
            )?;
        }
    }
    Ok(())
}

fn spawn_fund_invoice(
    rpc: FiberRpcClient,
    currency: FiberCurrency,
    terms: MatchTerms,
    term: HoldInvoiceTerm,
    invoice: String,
    tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) {
    tokio::spawn(async move {
        let result = async {
            let expectation = HoldInvoiceExpectation::new(
                &terms,
                &term,
                &terms.players[term.payee.index()].fiber_pubkey,
                currency,
            )?;
            rpc.fund_hold_invoice(&invoice, &expectation, terms.hold_payment_timeout_seconds)
                .await
        }
        .await;
        let message = match result {
            Ok(_) => ClientMessage::HoldInvoiceAck(ack_for(
                terms.match_id,
                &term,
                HoldInvoiceStage::Funded,
            )),
            Err(error) => ClientMessage::HoldInvoiceFailure(failure_for(
                terms.match_id,
                term.reservation_id,
                term.payment_hash,
                HoldInvoiceStage::Funded,
                error.to_string(),
            )),
        };
        tx.send(message).ok();
    });
}

fn spawn_settle_invoice(
    rpc: FiberRpcClient,
    terms: MatchTerms,
    term: HoldInvoiceTerm,
    preimage: [u8; 32],
    tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) {
    tokio::spawn(async move {
        let result = rpc
            .settle_hold_invoice(
                term.payment_hash,
                preimage,
                Duration::from_millis(terms.payment_deadline_ms),
            )
            .await;
        let message = match result {
            Ok(()) => ClientMessage::HoldInvoiceAck(ack_for(
                terms.match_id,
                &term,
                HoldInvoiceStage::Settled,
            )),
            Err(error) => ClientMessage::HoldInvoiceFailure(failure_for(
                terms.match_id,
                term.reservation_id,
                term.payment_hash,
                HoldInvoiceStage::Settled,
                error.to_string(),
            )),
        };
        tx.send(message).ok();
    });
}

fn spawn_cancel_invoice(
    rpc: FiberRpcClient,
    match_id: u128,
    term: HoldInvoiceTerm,
    tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) {
    tokio::spawn(async move {
        let result = rpc.cancel_hold_invoice(term.payment_hash).await;
        let message = match result {
            Ok(()) => {
                ClientMessage::HoldInvoiceAck(ack_for(match_id, &term, HoldInvoiceStage::Cancelled))
            }
            Err(error) => ClientMessage::HoldInvoiceFailure(failure_for(
                match_id,
                term.reservation_id,
                term.payment_hash,
                HoldInvoiceStage::Cancelled,
                error.to_string(),
            )),
        };
        tx.send(message).ok();
    });
}

fn checked_term<'a>(
    terms: &'a MatchTerms,
    offer: &HoldInvoiceOffer,
) -> Result<&'a HoldInvoiceTerm> {
    ensure!(
        offer.match_id == terms.match_id,
        "invoice offer for wrong match"
    );
    let term = terms
        .hold_invoices
        .iter()
        .find(|term| term.reservation_id == offer.reservation_id)
        .context("invoice offer for unknown reservation")?;
    ensure!(
        offer.payment_hash == term.payment_hash,
        "invoice offer has wrong payment hash"
    );
    Ok(term)
}

fn ack_for(match_id: u128, term: &HoldInvoiceTerm, stage: HoldInvoiceStage) -> HoldInvoiceAck {
    HoldInvoiceAck {
        match_id,
        reservation_id: term.reservation_id,
        payment_hash: term.payment_hash,
        stage,
    }
}

fn failure_for(
    match_id: u128,
    reservation_id: u16,
    payment_hash: [u8; 32],
    stage: HoldInvoiceStage,
    error: String,
) -> HoldInvoiceFailure {
    HoldInvoiceFailure {
        match_id,
        reservation_id,
        payment_hash,
        stage,
        error,
    }
}

fn send_control(network: &mut RenetClient, message: ClientMessage) -> Result<()> {
    let channel = if matches!(message, ClientMessage::Input(_)) {
        DefaultChannel::Unreliable
    } else {
        DefaultChannel::ReliableOrdered
    };
    network.send_message(channel, encode(&message)?);
    Ok(())
}
