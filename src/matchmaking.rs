use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use renet_netcode::ConnectToken;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::{protocol::PlayerSlot, security::decode_connect_token};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnterRoomRequest {
    pub name: String,
    pub fiber_pubkey: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketState {
    WaitingForOpponent,
    StartingServer,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoomTicket {
    pub room_code: String,
    pub ticket: String,
    pub slot: PlayerSlot,
    pub state: TicketState,
    pub expires_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_addr: Option<SocketAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RoomTicket {
    pub fn ready_connection(&self) -> Result<(SocketAddr, ConnectToken)> {
        if self.state != TicketState::Ready {
            bail!("room {} is not ready ({:?})", self.room_code, self.state);
        }
        let server = self
            .server_addr
            .context("ready room response omitted server_addr")?;
        let encoded = self
            .connect_token
            .as_deref()
            .context("ready room response omitted connect_token")?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .context("matchmaker returned malformed connect token encoding")?;
        let token = decode_connect_token(&bytes)
            .context("matchmaker returned an invalid or expired connect token")?;
        Ok((server, token))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchmakerHealth {
    pub status: String,
    pub waiting_rooms: usize,
    pub active_rooms: usize,
    pub available_endpoints: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[derive(Clone)]
pub struct MatchmakerClient {
    base_url: String,
    http: Client,
}

impl MatchmakerClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            bail!("matchmaker URL must start with http:// or https://");
        }
        Ok(Self {
            base_url,
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("building matchmaker HTTP client")?,
        })
    }

    pub async fn create_room(&self, request: &EnterRoomRequest) -> Result<RoomTicket> {
        self.response(
            self.http
                .post(format!("{}/v1/rooms", self.base_url))
                .json(request)
                .send()
                .await
                .context("creating matchmaker room")?,
        )
        .await
    }

    pub async fn join_room(
        &self,
        room_code: &str,
        request: &EnterRoomRequest,
    ) -> Result<RoomTicket> {
        self.response(
            self.http
                .post(format!(
                    "{}/v1/rooms/{}/join",
                    self.base_url,
                    room_code.trim().to_ascii_uppercase()
                ))
                .json(request)
                .send()
                .await
                .context("joining matchmaker room")?,
        )
        .await
    }

    pub async fn ticket(&self, room_code: &str, ticket: &str) -> Result<RoomTicket> {
        self.response(
            self.http
                .get(format!(
                    "{}/v1/rooms/{}",
                    self.base_url,
                    room_code.trim().to_ascii_uppercase()
                ))
                .bearer_auth(ticket)
                .send()
                .await
                .context("polling matchmaker room")?,
        )
        .await
    }

    pub async fn leave(&self, room_code: &str, ticket: &str) -> Result<()> {
        let response = self
            .http
            .delete(format!(
                "{}/v1/rooms/{}",
                self.base_url,
                room_code.trim().to_ascii_uppercase()
            ))
            .bearer_auth(ticket)
            .send()
            .await
            .context("leaving matchmaker room")?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let error = decode_error(response).await;
        bail!("{error}");
    }

    pub async fn wait_until_ready(
        &self,
        initial: RoomTicket,
        timeout: Duration,
    ) -> Result<RoomTicket> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut ticket = initial;
        loop {
            match ticket.state {
                TicketState::Ready => return Ok(ticket),
                TicketState::Failed => {
                    bail!(
                        "room {} failed: {}",
                        ticket.room_code,
                        ticket.error.as_deref().unwrap_or("unknown server error")
                    );
                }
                TicketState::WaitingForOpponent | TicketState::StartingServer => {}
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timed out waiting for room {}", ticket.room_code);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            ticket = self.ticket(&ticket.room_code, &ticket.ticket).await?;
        }
    }

    async fn response(&self, response: reqwest::Response) -> Result<RoomTicket> {
        if response.status().is_success() {
            return response
                .json()
                .await
                .context("decoding matchmaker response");
        }
        let error = decode_error(response).await;
        bail!("{error}");
    }
}

async fn decode_error(response: reqwest::Response) -> String {
    let status = response.status();
    match response.json::<ApiError>().await {
        Ok(error) => format!("matchmaker {}: {} ({})", error.code, error.message, status),
        Err(error) => format!("matchmaker request failed with {status}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matchmaker_url_requires_http() {
        assert!(MatchmakerClient::new("https://arena.example").is_ok());
        assert!(MatchmakerClient::new("arena.example").is_err());
    }
}
