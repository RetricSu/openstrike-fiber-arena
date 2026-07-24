use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::Parser;
use openstrike_fiber_arena::{
    matchmaking::{ApiError, EnterRoomRequest, MatchmakerHealth, RoomTicket, TicketState},
    net::{unix_ms, validate_player_binding},
    protocol::{PlayerBinding, PlayerSlot},
    security::{issue_connect_token, load_secret_32, validate_private_file},
};
use rand_core::{OsRng, RngCore};
use tracing::{error, info, warn};

const ROOM_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GameEndpoint {
    local_port: u16,
    public_addr: SocketAddr,
}

impl FromStr for GameEndpoint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (local_port, public_addr) = value
            .split_once('=')
            .ok_or_else(|| "endpoint must be LOCAL_PORT=PUBLIC_HOST:PORT".to_owned())?;
        let local_port = local_port
            .parse::<u16>()
            .map_err(|_| "local game port must be between 1 and 65535".to_owned())?;
        if local_port == 0 {
            return Err("local game port must be between 1 and 65535".into());
        }
        let public_addr = match public_addr.parse::<SocketAddr>() {
            Ok(address) => address,
            Err(_) => {
                let addresses: Vec<_> = public_addr
                    .to_socket_addrs()
                    .map_err(|_| "public game endpoint must be a resolvable host:port".to_owned())?
                    .collect();
                addresses
                    .iter()
                    .copied()
                    .find(SocketAddr::is_ipv4)
                    .or_else(|| addresses.first().copied())
                    .ok_or_else(|| "public game endpoint resolved to no addresses".to_owned())?
            }
        };
        if public_addr.ip().is_unspecified() || public_addr.port() == 0 {
            return Err("public game endpoint must be reachable".into());
        }
        Ok(Self {
            local_port,
            public_addr,
        })
    }
}

#[derive(Debug, Parser)]
#[command(about = "HTTP room service and per-match arena-server supervisor")]
struct Args {
    /// HTTP address. Keep this on loopback when Cloudflare Tunnel is the ingress.
    #[arg(long, default_value = "127.0.0.1:8080")]
    http_bind: SocketAddr,
    /// Preconfigured UDP tunnel: LOCAL_PORT=PUBLIC_HOST:PORT. Repeat per room slot.
    #[arg(long = "game-endpoint", required = true)]
    game_endpoints: Vec<GameEndpoint>,
    /// Local address used by child game servers.
    #[arg(long, default_value = "0.0.0.0")]
    game_bind_ip: IpAddr,
    #[arg(long)]
    netcode_key: PathBuf,
    #[arg(long)]
    signing_key_file: PathBuf,
    /// arena-server binary. Defaults to a sibling of arena-matchmaker.
    #[arg(long)]
    server_bin: Option<PathBuf>,
    #[arg(long, default_value = "logs/matches")]
    log_dir: PathBuf,
    #[arg(long)]
    map: Option<PathBuf>,
    #[arg(long = "wad-dir", requires = "map")]
    wad_dirs: Vec<PathBuf>,
    #[arg(long, default_value_t = 300)]
    waiting_ttl_seconds: u64,
    #[arg(long, default_value_t = 3_600)]
    match_ttl_seconds: u64,
    #[arg(long, default_value_t = 600)]
    token_ttl_seconds: u64,
    #[arg(long, default_value_t = 15)]
    token_timeout_seconds: i32,
    #[arg(long, default_value_t = 60)]
    failed_retention_seconds: u64,
    #[arg(long, default_value_t = 128)]
    max_waiting_rooms: usize,
    #[arg(long, default_value_t = 25)]
    damage_bucket: u16,
    #[arg(long, default_value_t = 1_000)]
    amount_per_bucket: u128,
    #[arg(long, default_value_t = 4_000)]
    max_total_per_player: u128,
    #[arg(long, default_value = "info")]
    game_rust_log: String,
}

#[derive(Clone)]
struct Config {
    endpoints: Vec<GameEndpoint>,
    game_bind_ip: IpAddr,
    netcode_key: [u8; 32],
    netcode_key_file: PathBuf,
    signing_key_file: PathBuf,
    server_bin: PathBuf,
    log_dir: PathBuf,
    map: Option<PathBuf>,
    wad_dirs: Vec<PathBuf>,
    waiting_ttl_ms: u64,
    match_ttl_ms: u64,
    token_ttl_seconds: u64,
    token_timeout_seconds: i32,
    failed_retention_ms: u64,
    max_waiting_rooms: usize,
    damage_bucket: u16,
    amount_per_bucket: u128,
    max_total_per_player: u128,
    game_rust_log: String,
}

#[derive(Clone)]
struct PlayerEntry {
    binding: PlayerBinding,
    slot: PlayerSlot,
    ticket: String,
    connect_token: Option<String>,
}

struct Room {
    code: String,
    state: TicketState,
    expires_at_unix_ms: u64,
    players: [Option<PlayerEntry>; 2],
    endpoint: Option<GameEndpoint>,
    child: Option<Child>,
    error: Option<String>,
}

#[derive(Debug)]
struct StartPlan {
    room_code: String,
    endpoint: GameEndpoint,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    rooms: Arc<Mutex<HashMap<String, Room>>>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            rooms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn rooms(&self) -> Result<MutexGuard<'_, HashMap<String, Room>>, HttpError> {
        self.rooms
            .lock()
            .map_err(|_| HttpError::internal("room state lock was poisoned"))
    }

    fn create_room(&self, request: EnterRoomRequest) -> Result<RoomTicket, HttpError> {
        let binding = valid_binding(request)?;
        let now = unix_ms();
        let mut rooms = self.rooms()?;
        reap_locked(&self.config, &mut rooms, now);
        let waiting = rooms
            .values()
            .filter(|room| room.state == TicketState::WaitingForOpponent)
            .count();
        if waiting >= self.config.max_waiting_rooms {
            return Err(HttpError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "waiting_room_limit",
                "too many rooms are waiting for an opponent",
            ));
        }
        let code = unique_room_code(&rooms)?;
        let player = PlayerEntry {
            binding,
            slot: PlayerSlot::A,
            ticket: random_secret(),
            connect_token: None,
        };
        let response = room_ticket(
            &Room {
                code: code.clone(),
                state: TicketState::WaitingForOpponent,
                expires_at_unix_ms: now.saturating_add(self.config.waiting_ttl_ms),
                players: [Some(player.clone()), None],
                endpoint: None,
                child: None,
                error: None,
            },
            &player.ticket,
        )?;
        rooms.insert(
            code.clone(),
            Room {
                code,
                state: TicketState::WaitingForOpponent,
                expires_at_unix_ms: response.expires_at_unix_ms,
                players: [Some(player), None],
                endpoint: None,
                child: None,
                error: None,
            },
        );
        Ok(response)
    }

    fn reserve_join(
        &self,
        room_code: &str,
        request: EnterRoomRequest,
    ) -> Result<(RoomTicket, StartPlan), HttpError> {
        let binding = valid_binding(request)?;
        let room_code = normalize_room_code(room_code)?;
        let now = unix_ms();
        let mut rooms = self.rooms()?;
        reap_locked(&self.config, &mut rooms, now);

        let room = rooms.get(&room_code).ok_or_else(|| {
            HttpError::new(
                StatusCode::NOT_FOUND,
                "room_not_found",
                "room was not found",
            )
        })?;
        if room.state != TicketState::WaitingForOpponent || room.players[1].is_some() {
            return Err(HttpError::new(
                StatusCode::CONFLICT,
                "room_full",
                "room already has two players",
            ));
        }
        let first = room.players[0]
            .as_ref()
            .expect("a waiting room always has its creator");
        if first.binding.fiber_pubkey == binding.fiber_pubkey {
            return Err(HttpError::new(
                StatusCode::CONFLICT,
                "duplicate_fiber_identity",
                "both seats cannot use the same Fiber identity",
            ));
        }
        if first.binding.name == binding.name {
            return Err(HttpError::new(
                StatusCode::CONFLICT,
                "duplicate_player_name",
                "both seats cannot use the same player name",
            ));
        }
        let used_endpoints: HashSet<u16> = rooms
            .values()
            .filter(|room| matches!(room.state, TicketState::StartingServer | TicketState::Ready))
            .filter_map(|room| room.endpoint.map(|endpoint| endpoint.local_port))
            .collect();
        let endpoint = self
            .config
            .endpoints
            .iter()
            .copied()
            .find(|endpoint| !used_endpoints.contains(&endpoint.local_port))
            .ok_or_else(|| {
                HttpError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no_game_endpoint",
                    "all game server slots are currently occupied",
                )
            })?;

        let room = rooms
            .get_mut(&room_code)
            .expect("room was validated while the state lock was held");
        let second = PlayerEntry {
            binding,
            slot: PlayerSlot::B,
            ticket: random_secret(),
            connect_token: None,
        };
        room.players[1] = Some(second);
        room.endpoint = Some(endpoint);
        room.state = TicketState::StartingServer;
        room.expires_at_unix_ms = now.saturating_add(self.config.match_ttl_ms);

        for index in 0..room.players.len() {
            let player = room.players[index]
                .as_mut()
                .expect("a starting room has both players");
            let (bytes, _, _) = issue_connect_token(
                &self.config.netcode_key,
                endpoint.public_addr,
                player.binding.name.clone(),
                player.slot,
                player.binding.fiber_pubkey.clone(),
                None,
                self.config.token_ttl_seconds,
                self.config.token_timeout_seconds,
            )
            .map_err(|error| {
                HttpError::internal(format!("could not issue connect token: {error:#}"))
            })?;
            player.connect_token = Some(URL_SAFE_NO_PAD.encode(bytes));
        }

        let second_ticket = room.players[1]
            .as_ref()
            .expect("second player was just inserted")
            .ticket
            .clone();
        let response = room_ticket(room, &second_ticket)?;
        Ok((
            response,
            StartPlan {
                room_code,
                endpoint,
            },
        ))
    }

    fn mark_ready(&self, room_code: &str, child: Child) -> Result<(), HttpError> {
        let mut rooms = self.rooms()?;
        let Some(room) = rooms.get_mut(room_code) else {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(HttpError::internal(
                "room disappeared while its server was starting",
            ));
        };
        room.child = Some(child);
        room.state = TicketState::Ready;
        Ok(())
    }

    fn mark_failed(&self, room_code: &str, message: String) {
        let Ok(mut rooms) = self.rooms.lock() else {
            return;
        };
        if let Some(room) = rooms.get_mut(room_code) {
            room.state = TicketState::Failed;
            room.error = Some(message);
            room.expires_at_unix_ms = unix_ms().saturating_add(self.config.failed_retention_ms);
            clear_connect_tokens(room);
            stop_child(room);
        }
    }

    fn ticket(&self, room_code: &str, ticket: &str) -> Result<RoomTicket, HttpError> {
        let room_code = normalize_room_code(room_code)?;
        let now = unix_ms();
        let mut rooms = self.rooms()?;
        reap_locked(&self.config, &mut rooms, now);
        let room = rooms.get(&room_code).ok_or_else(|| {
            HttpError::new(
                StatusCode::NOT_FOUND,
                "room_not_found",
                "room was not found or has expired",
            )
        })?;
        room_ticket(room, ticket)
    }

    fn leave(&self, room_code: &str, ticket: &str) -> Result<(), HttpError> {
        let room_code = normalize_room_code(room_code)?;
        let mut rooms = self.rooms()?;
        let room = rooms.get_mut(&room_code).ok_or_else(|| {
            HttpError::new(
                StatusCode::NOT_FOUND,
                "room_not_found",
                "room was not found",
            )
        })?;
        authenticate_ticket(room, ticket)?;
        if room.state == TicketState::WaitingForOpponent {
            rooms.remove(&room_code);
        } else {
            room.state = TicketState::Failed;
            room.error = Some("a player left before the match completed".into());
            room.expires_at_unix_ms = unix_ms().saturating_add(self.config.failed_retention_ms);
            clear_connect_tokens(room);
            stop_child(room);
        }
        Ok(())
    }

    fn health(&self) -> Result<MatchmakerHealth, HttpError> {
        let now = unix_ms();
        let mut rooms = self.rooms()?;
        reap_locked(&self.config, &mut rooms, now);
        let waiting_rooms = rooms
            .values()
            .filter(|room| room.state == TicketState::WaitingForOpponent)
            .count();
        let active_rooms = rooms
            .values()
            .filter(|room| matches!(room.state, TicketState::StartingServer | TicketState::Ready))
            .count();
        Ok(MatchmakerHealth {
            status: "ok".into(),
            waiting_rooms,
            active_rooms,
            available_endpoints: self.config.endpoints.len().saturating_sub(active_rooms),
        })
    }

    fn spawn_game(&self, plan: &StartPlan) -> Result<Child> {
        fs::create_dir_all(&self.config.log_dir).with_context(|| {
            format!(
                "creating match log directory {}",
                self.config.log_dir.display()
            )
        })?;
        let log_path = self
            .config
            .log_dir
            .join(format!("room-{}.log", plan.room_code));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening match log {}", log_path.display()))?;
        let stderr = log
            .try_clone()
            .with_context(|| format!("cloning match log {}", log_path.display()))?;
        let bind_addr = SocketAddr::new(self.config.game_bind_ip, plan.endpoint.local_port);
        let mut command = Command::new(&self.config.server_bin);
        command
            .arg("--bind")
            .arg(bind_addr.to_string())
            .arg("--public-addr")
            .arg(plan.endpoint.public_addr.to_string())
            .arg("--netcode-key")
            .arg(&self.config.netcode_key_file)
            .arg("--signing-key-file")
            .arg(&self.config.signing_key_file)
            .arg("--damage-bucket")
            .arg(self.config.damage_bucket.to_string())
            .arg("--amount-per-bucket")
            .arg(self.config.amount_per_bucket.to_string())
            .arg("--max-total-per-player")
            .arg(self.config.max_total_per_player.to_string())
            .arg("--exit-after-match-ms")
            .arg("5000")
            .arg("--exit-when-empty-ms")
            .arg("30000")
            .arg("--exit-if-no-clients-ms")
            .arg(
                self.config
                    .token_ttl_seconds
                    .saturating_mul(1_000)
                    .to_string(),
            )
            .env("RUST_LOG", &self.config.game_rust_log)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        if let Some(map) = &self.config.map {
            command.arg("--map").arg(map);
            for wad_dir in &self.config.wad_dirs {
                command.arg("--wad-dir").arg(wad_dir);
            }
        } else {
            command.arg("--dev-arena");
        }
        command.spawn().with_context(|| {
            format!(
                "starting {} for room {} on {}",
                self.config.server_bin.display(),
                plan.room_code,
                bind_addr
            )
        })
    }

    fn shutdown(&self) {
        let Ok(mut rooms) = self.rooms.lock() else {
            return;
        };
        for room in rooms.values_mut() {
            stop_child(room);
        }
        rooms.clear();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    openstrike_fiber_arena::net::init_tracing();
    let args = Args::parse();
    let config = validate_args(args)?;
    let http_bind = config.0;
    let state = AppState::new(config.1);
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/rooms", post(create_room))
        .route("/v1/rooms/{room_code}/join", post(join_room))
        .route(
            "/v1/rooms/{room_code}",
            get(get_ticket)
                .delete(leave_room)
                .route_layer(DefaultBodyLimit::disable()),
        )
        .layer(DefaultBodyLimit::max(4 * 1024))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(http_bind)
        .await
        .with_context(|| format!("binding HTTP matchmaker to {http_bind}"))?;
    let cleanup_state = state.clone();
    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            if let Err(error) = cleanup_state.health() {
                error!(%error, "matchmaker cleanup failed");
            }
        }
    });
    info!(
        bind = %http_bind,
        endpoints = state.config.endpoints.len(),
        "arena matchmaker ready"
    );
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP matchmaker");
    cleanup.abort();
    state.shutdown();
    result
}

async fn health(State(state): State<AppState>) -> Result<Json<MatchmakerHealth>, HttpError> {
    state.health().map(Json)
}

async fn create_room(
    State(state): State<AppState>,
    Json(request): Json<EnterRoomRequest>,
) -> Result<Response, HttpError> {
    let ticket = state.create_room(request)?;
    info!(room = %ticket.room_code, "room created");
    Ok(json_no_store(StatusCode::CREATED, ticket))
}

async fn join_room(
    State(state): State<AppState>,
    Path(room_code): Path<String>,
    Json(request): Json<EnterRoomRequest>,
) -> Result<Response, HttpError> {
    let (mut ticket, plan) = state.reserve_join(&room_code, request)?;
    info!(
        room = %plan.room_code,
        local_port = plan.endpoint.local_port,
        public_addr = %plan.endpoint.public_addr,
        "room filled; starting game server"
    );
    match state.spawn_game(&plan) {
        Ok(mut child) => {
            tokio::time::sleep(Duration::from_millis(150)).await;
            match child.try_wait() {
                Ok(None) => {
                    state.mark_ready(&plan.room_code, child)?;
                    ticket = state.ticket(&plan.room_code, &ticket.ticket)?;
                }
                Ok(Some(status)) => {
                    let message = format!("game server exited during startup with {status}");
                    state.mark_failed(&plan.room_code, message.clone());
                    return Err(HttpError::internal(message));
                }
                Err(error) => {
                    let message = format!("could not inspect game server startup: {error}");
                    let _ = child.kill();
                    let _ = child.wait();
                    state.mark_failed(&plan.room_code, message.clone());
                    return Err(HttpError::internal(message));
                }
            }
        }
        Err(error) => {
            let message = format!("could not start game server: {error:#}");
            state.mark_failed(&plan.room_code, message.clone());
            return Err(HttpError::internal(message));
        }
    }
    Ok(json_no_store(StatusCode::OK, ticket))
}

async fn get_ticket(
    State(state): State<AppState>,
    Path(room_code): Path<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let ticket = bearer_ticket(&headers)?;
    Ok(json_no_store(
        StatusCode::OK,
        state.ticket(&room_code, ticket)?,
    ))
}

async fn leave_room(
    State(state): State<AppState>,
    Path(room_code): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, HttpError> {
    state.leave(&room_code, bearer_ticket(&headers)?)?;
    Ok(StatusCode::NO_CONTENT)
}

fn json_no_store(status: StatusCode, value: RoomTicket) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn validate_args(args: Args) -> Result<(SocketAddr, Config)> {
    ensure!(
        !args.game_endpoints.is_empty(),
        "at least one --game-endpoint is required"
    );
    ensure!(
        args.waiting_ttl_seconds > 0 && args.match_ttl_seconds > 0,
        "room TTLs must be positive"
    );
    ensure!(
        (1..=3_600).contains(&args.token_ttl_seconds),
        "--token-ttl-seconds must be between 1 and 3600"
    );
    ensure!(
        args.token_timeout_seconds > 0,
        "--token-timeout-seconds must be positive"
    );
    ensure!(
        args.match_ttl_seconds >= args.token_ttl_seconds,
        "--match-ttl-seconds must be at least --token-ttl-seconds"
    );
    ensure!(
        args.max_waiting_rooms > 0,
        "--max-waiting-rooms must be positive"
    );
    ensure!(
        args.damage_bucket > 0 && args.amount_per_bucket > 0,
        "damage bucket and payment amount must be positive"
    );
    ensure!(
        args.max_total_per_player >= args.amount_per_bucket
            && args
                .max_total_per_player
                .is_multiple_of(args.amount_per_bucket),
        "--max-total-per-player must be a positive multiple of --amount-per-bucket"
    );
    let mut local_ports = HashSet::new();
    let mut public_addresses = HashSet::new();
    for endpoint in &args.game_endpoints {
        ensure!(
            local_ports.insert(endpoint.local_port),
            "duplicate local game port {}",
            endpoint.local_port
        );
        ensure!(
            public_addresses.insert(endpoint.public_addr),
            "duplicate public game endpoint {}",
            endpoint.public_addr
        );
    }
    let netcode_key = load_secret_32(&args.netcode_key, "Netcode private key")?;
    validate_private_file(&args.signing_key_file, "settlement signing key")?;
    let server_bin = match args.server_bin {
        Some(path) => path,
        None => std::env::current_exe()
            .context("finding arena-matchmaker executable")?
            .parent()
            .context("arena-matchmaker executable has no parent directory")?
            .join("arena-server"),
    };
    ensure!(
        server_bin.is_file(),
        "arena-server binary not found: {}",
        server_bin.display()
    );
    if let Some(map) = &args.map {
        ensure!(map.is_file(), "map not found: {}", map.display());
        for wad_dir in &args.wad_dirs {
            ensure!(
                wad_dir.is_dir(),
                "WAD directory not found: {}",
                wad_dir.display()
            );
        }
    }
    fs::create_dir_all(&args.log_dir)
        .with_context(|| format!("creating log directory {}", args.log_dir.display()))?;
    Ok((
        args.http_bind,
        Config {
            endpoints: args.game_endpoints,
            game_bind_ip: args.game_bind_ip,
            netcode_key,
            netcode_key_file: args.netcode_key,
            signing_key_file: args.signing_key_file,
            server_bin,
            log_dir: args.log_dir,
            map: args.map,
            wad_dirs: args.wad_dirs,
            waiting_ttl_ms: args.waiting_ttl_seconds.saturating_mul(1_000),
            match_ttl_ms: args.match_ttl_seconds.saturating_mul(1_000),
            token_ttl_seconds: args.token_ttl_seconds,
            token_timeout_seconds: args.token_timeout_seconds,
            failed_retention_ms: args.failed_retention_seconds.saturating_mul(1_000),
            max_waiting_rooms: args.max_waiting_rooms,
            damage_bucket: args.damage_bucket,
            amount_per_bucket: args.amount_per_bucket,
            max_total_per_player: args.max_total_per_player,
            game_rust_log: args.game_rust_log,
        },
    ))
}

fn valid_binding(request: EnterRoomRequest) -> Result<PlayerBinding, HttpError> {
    validate_player_binding(&PlayerBinding {
        name: request.name,
        fiber_pubkey: request.fiber_pubkey,
    })
    .map_err(|error| HttpError::new(StatusCode::BAD_REQUEST, "invalid_player", error))
}

fn bearer_ticket(headers: &HeaderMap) -> Result<&str, HttpError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|ticket| !ticket.is_empty())
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::UNAUTHORIZED,
                "ticket_required",
                "send the private room ticket as an Authorization bearer token",
            )
        })?;
    Ok(value)
}

fn normalize_room_code(room_code: &str) -> Result<String, HttpError> {
    let room_code = room_code.trim().to_ascii_uppercase();
    if room_code.len() != 8 || !room_code.bytes().all(|byte| ROOM_ALPHABET.contains(&byte)) {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_room_code",
            "room code must contain eight supported uppercase characters",
        ));
    }
    Ok(room_code)
}

fn unique_room_code(rooms: &HashMap<String, Room>) -> Result<String, HttpError> {
    for _ in 0..64 {
        let mut bytes = [0u8; 8];
        OsRng.fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|byte| ROOM_ALPHABET[*byte as usize % ROOM_ALPHABET.len()] as char)
            .collect();
        if !rooms.contains_key(&code) {
            return Ok(code);
        }
    }
    Err(HttpError::internal("could not allocate a unique room code"))
}

fn random_secret() -> String {
    let mut bytes = [0u8; 24];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn authenticate_ticket<'a>(room: &'a Room, ticket: &str) -> Result<&'a PlayerEntry, HttpError> {
    room.players
        .iter()
        .flatten()
        .find(|player| player.ticket == ticket)
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::NOT_FOUND,
                "ticket_not_found",
                "room ticket was not found",
            )
        })
}

fn room_ticket(room: &Room, ticket: &str) -> Result<RoomTicket, HttpError> {
    let player = authenticate_ticket(room, ticket)?;
    let ready = room.state == TicketState::Ready;
    Ok(RoomTicket {
        room_code: room.code.clone(),
        ticket: player.ticket.clone(),
        slot: player.slot,
        state: room.state,
        expires_at_unix_ms: room.expires_at_unix_ms,
        server_addr: ready.then(|| {
            room.endpoint
                .expect("a ready room has a game endpoint")
                .public_addr
        }),
        connect_token: ready.then(|| player.connect_token.clone()).flatten(),
        error: room.error.clone(),
    })
}

fn reap_locked(config: &Config, rooms: &mut HashMap<String, Room>, now: u64) {
    for room in rooms.values_mut() {
        let exited = match room.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    warn!(room = %room.code, %error, "could not inspect game server");
                    let _ = child.kill();
                    Some(child.wait().unwrap_or_else(|_| failure_exit_status()))
                }
            },
            None => None,
        };
        if let Some(status) = exited {
            room.child = None;
            if room.state == TicketState::Ready {
                room.state = TicketState::Failed;
                room.error = Some(format!("game server exited with {status}"));
                room.expires_at_unix_ms = now.saturating_add(config.failed_retention_ms);
                clear_connect_tokens(room);
            }
        }
    }
    rooms.retain(|code, room| {
        if room.expires_at_unix_ms > now {
            return true;
        }
        info!(room = %code, "room expired");
        stop_child(room);
        false
    });
}

#[cfg(unix)]
fn failure_exit_status() -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(1 << 8)
}

#[cfg(windows)]
fn failure_exit_status() -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(1)
}

fn stop_child(room: &mut Room) {
    if let Some(mut child) = room.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn clear_connect_tokens(room: &mut Room) {
    for player in room.players.iter_mut().flatten() {
        player.connect_token = None;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl HttpError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let Self {
            status,
            code,
            message,
        } = self;
        let public_message = if status.is_server_error() {
            error!(code, %message, "matchmaker request failed");
            "internal matchmaker error".to_owned()
        } else {
            message
        };
        let mut response = (
            status,
            Json(ApiError {
                code: code.into(),
                message: public_message,
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openstrike_fiber_arena::net::{MOCK_FIBER_PUBKEY_A, MOCK_FIBER_PUBKEY_B};
    use std::net::Ipv4Addr;

    fn test_state(endpoint_count: u16) -> AppState {
        let endpoints = (0..endpoint_count)
            .map(|offset| GameEndpoint {
                local_port: 5100 + offset,
                public_addr: format!("127.0.0.1:{}", 6100 + offset).parse().unwrap(),
            })
            .collect();
        AppState::new(Config {
            endpoints,
            game_bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            netcode_key: [7; 32],
            netcode_key_file: "netcode.key".into(),
            signing_key_file: "signing.key".into(),
            server_bin: "arena-server".into(),
            log_dir: "logs".into(),
            map: None,
            wad_dirs: vec![],
            waiting_ttl_ms: 300_000,
            match_ttl_ms: 3_600_000,
            token_ttl_seconds: 600,
            token_timeout_seconds: 15,
            failed_retention_ms: 60_000,
            max_waiting_rooms: 8,
            damage_bucket: 25,
            amount_per_bucket: 1_000,
            max_total_per_player: 4_000,
            game_rust_log: "info".into(),
        })
    }

    fn player(name: &str, pubkey: &str) -> EnterRoomRequest {
        EnterRoomRequest {
            name: name.into(),
            fiber_pubkey: pubkey.into(),
        }
    }

    #[test]
    fn room_binds_two_distinct_fiber_players_and_rejects_a_third() {
        let state = test_state(1);
        let alice = state
            .create_room(player("alice", MOCK_FIBER_PUBKEY_A))
            .unwrap();
        assert_eq!(alice.slot, PlayerSlot::A);
        assert_eq!(alice.state, TicketState::WaitingForOpponent);
        let (bob, plan) = state
            .reserve_join(&alice.room_code, player("bob", MOCK_FIBER_PUBKEY_B))
            .unwrap();
        assert_eq!(bob.slot, PlayerSlot::B);
        assert_eq!(bob.state, TicketState::StartingServer);
        assert_eq!(plan.endpoint.local_port, 5100);
        let error = state
            .reserve_join(
                &alice.room_code,
                player(
                    "carol",
                    "03f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
                ),
            )
            .unwrap_err();
        assert_eq!(error.code, "room_full");
    }

    #[test]
    fn endpoint_pool_limits_concurrent_rooms() {
        let state = test_state(1);
        let first = state
            .create_room(player("alice", MOCK_FIBER_PUBKEY_A))
            .unwrap();
        state
            .reserve_join(&first.room_code, player("bob", MOCK_FIBER_PUBKEY_B))
            .unwrap();
        let second = state
            .create_room(player(
                "carol",
                "03f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
            ))
            .unwrap();
        let error = state
            .reserve_join(
                &second.room_code,
                player(
                    "dave",
                    "02e493dbf1c10d80f3581e4904930b1404cc6c13900ee0758474fa94abe8c4cd13",
                ),
            )
            .unwrap_err();
        assert_eq!(error.code, "no_game_endpoint");
    }

    #[test]
    fn room_rejects_duplicate_fiber_identity() {
        let state = test_state(1);
        let room = state
            .create_room(player("alice", MOCK_FIBER_PUBKEY_A))
            .unwrap();
        let error = state
            .reserve_join(&room.room_code, player("mallory", MOCK_FIBER_PUBKEY_A))
            .unwrap_err();
        assert_eq!(error.code, "duplicate_fiber_identity");
    }

    #[test]
    fn endpoint_parser_accepts_public_ip_or_hostname_and_port() {
        assert_eq!(
            "5000=127.0.0.1:6000"
                .parse::<GameEndpoint>()
                .unwrap()
                .local_port,
            5000
        );
        assert!("5000=localhost:6000".parse::<GameEndpoint>().is_ok());
        assert!("0=127.0.0.1:6000".parse::<GameEndpoint>().is_err());
    }
}
