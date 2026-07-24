#![cfg(all(unix, feature = "openstrike"))]

use std::{
    fs,
    net::{TcpListener, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use openstrike_fiber_arena::{
    matchmaking::{ApiError, EnterRoomRequest, MatchmakerHealth, RoomTicket, TicketState},
    net::{MOCK_FIBER_PUBKEY_A, MOCK_FIBER_PUBKEY_B},
    protocol::PlayerSlot,
    security::write_private_file,
};
use reqwest::{Client, StatusCode};

struct ProcessGuard {
    child: Child,
    directory: PathBuf,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn create_join_poll_reject_third_and_recycle_endpoint() {
    let directory =
        std::env::temp_dir().join(format!("arena-matchmaker-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let netcode_key = directory.join("netcode.key");
    let signing_key = directory.join("signing.key");
    write_private_file(
        &netcode_key,
        format!("{}\n", hex::encode([7; 32])).as_bytes(),
    )
    .unwrap();
    write_private_file(
        &signing_key,
        format!("{}\n", hex::encode([8; 32])).as_bytes(),
    )
    .unwrap();

    let http_port = unused_tcp_port();
    let game_port = unused_udp_port();
    let child = Command::new(env!("CARGO_BIN_EXE_arena-matchmaker"))
        .arg("--http-bind")
        .arg(format!("127.0.0.1:{http_port}"))
        .arg("--game-endpoint")
        .arg(format!("{game_port}=127.0.0.1:{game_port}"))
        .arg("--netcode-key")
        .arg(&netcode_key)
        .arg("--signing-key-file")
        .arg(&signing_key)
        .arg("--server-bin")
        .arg(env!("CARGO_BIN_EXE_arena-server"))
        .arg("--log-dir")
        .arg(directory.join("logs"))
        .arg("--failed-retention-seconds")
        .arg("2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ProcessGuard {
        child,
        directory: directory.clone(),
    };

    let base_url = format!("http://127.0.0.1:{http_port}");
    let http = Client::new();
    wait_for_health(&http, &base_url).await;
    let index = http
        .get(&base_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(index.contains("matchmaker is online"));

    let alice = create(
        &http,
        &base_url,
        EnterRoomRequest {
            name: "alice".into(),
            fiber_pubkey: MOCK_FIBER_PUBKEY_A.into(),
        },
    )
    .await;
    assert_eq!(alice.slot, PlayerSlot::A);
    assert_eq!(alice.state, TicketState::WaitingForOpponent);
    let unauthenticated = http
        .get(format!("{base_url}/v1/rooms/{}", alice.room_code))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthenticated.json::<ApiError>().await.unwrap().code,
        "ticket_required"
    );

    let bob = join(
        &http,
        &base_url,
        &alice.room_code,
        EnterRoomRequest {
            name: "bob".into(),
            fiber_pubkey: MOCK_FIBER_PUBKEY_B.into(),
        },
    )
    .await;
    assert_eq!(bob.slot, PlayerSlot::B);
    assert_eq!(bob.state, TicketState::Ready);
    assert_eq!(
        bob.ready_connection().unwrap().0,
        format!("127.0.0.1:{game_port}").parse().unwrap()
    );

    let alice_ready: RoomTicket = http
        .get(format!("{base_url}/v1/rooms/{}", alice.room_code))
        .bearer_auth(&alice.ticket)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(alice_ready.state, TicketState::Ready);
    assert!(alice_ready.ready_connection().is_ok());

    let third = http
        .post(format!("{base_url}/v1/rooms/{}/join", alice.room_code))
        .json(&EnterRoomRequest {
            name: "carol".into(),
            fiber_pubkey: "03f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
                .into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), StatusCode::CONFLICT);
    assert_eq!(third.json::<ApiError>().await.unwrap().code, "room_full");

    let alice_token = directory.join("alice.token");
    let bob_token = directory.join("bob.token");
    write_ticket_token(&alice_ready, &alice_token);
    write_ticket_token(&bob, &bob_token);
    let mut alice_client = headless_client("alice", game_port, &alice_token);
    let mut bob_client = headless_client("bob", game_port, &bob_token);
    wait_for_success(&mut alice_client).await;
    wait_for_success(&mut bob_client).await;
    wait_for_available_endpoint(&http, &base_url).await;

    let second_room = create(
        &http,
        &base_url,
        EnterRoomRequest {
            name: "carol".into(),
            fiber_pubkey: "03f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9"
                .into(),
        },
    )
    .await;
    let dave = join(
        &http,
        &base_url,
        &second_room.room_code,
        EnterRoomRequest {
            name: "dave".into(),
            fiber_pubkey: "02e493dbf1c10d80f3581e4904930b1404cc6c13900ee0758474fa94abe8c4cd13"
                .into(),
        },
    )
    .await;
    assert_eq!(dave.state, TicketState::Ready);
}

async fn create(http: &Client, base_url: &str, request: EnterRoomRequest) -> RoomTicket {
    let response = http
        .post(format!("{base_url}/v1/rooms"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
    response.json().await.unwrap()
}

async fn join(
    http: &Client,
    base_url: &str,
    room_code: &str,
    request: EnterRoomRequest,
) -> RoomTicket {
    http.post(format!("{base_url}/v1/rooms/{room_code}/join"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_for_health(http: &Client, base_url: &str) {
    for _ in 0..80 {
        if let Ok(response) = http.get(format!("{base_url}/healthz")).send().await
            && response.status().is_success()
        {
            let health: MatchmakerHealth = response.json().await.unwrap();
            assert_eq!(health.available_endpoints, 1);
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("matchmaker did not become healthy");
}

async fn wait_for_available_endpoint(http: &Client, base_url: &str) {
    for _ in 0..160 {
        if let Ok(response) = http.get(format!("{base_url}/healthz")).send().await
            && response.status().is_success()
        {
            let health: MatchmakerHealth = response.json().await.unwrap();
            if health.active_rooms == 0 && health.available_endpoints == 1 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("completed match did not release its game endpoint");
}

fn write_ticket_token(ticket: &RoomTicket, path: &Path) {
    let bytes = URL_SAFE_NO_PAD
        .decode(ticket.connect_token.as_deref().unwrap())
        .unwrap();
    write_private_file(path, &bytes).unwrap();
}

fn headless_client(name: &str, game_port: u16, token: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_arena-client"))
        .arg("--server")
        .arg(format!("127.0.0.1:{game_port}"))
        .arg("--name")
        .arg(name)
        .arg("--connect-token")
        .arg(token)
        .arg("--mock-payments")
        .arg("--auto-fire")
        .arg("--exit-on-end")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

async fn wait_for_success(child: &mut Child) {
    for _ in 0..300 {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "headless client failed with {status}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("headless client did not finish");
}

fn unused_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn unused_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
