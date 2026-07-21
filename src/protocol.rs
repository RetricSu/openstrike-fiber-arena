use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[repr(u8)]
pub enum PlayerSlot {
    #[default]
    A,
    B,
}

impl PlayerSlot {
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn opponent(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct InputFrame {
    pub sequence: u32,
    pub client_tick: u64,
    pub move_x: f32,
    pub move_y: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub walk: bool,
    pub jump: bool,
    pub fire: bool,
    pub reload: bool,
}

impl InputFrame {
    pub fn sanitized(mut self) -> Self {
        self.move_x = finite_or_zero(self.move_x).clamp(-1.0, 1.0);
        self.move_y = finite_or_zero(self.move_y).clamp(-1.0, 1.0);
        self.yaw = finite_or_zero(self.yaw);
        self.pitch = finite_or_zero(self.pitch).clamp(-1.553_343, 1.553_343);
        self
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MatchPhase {
    Waiting,
    Live,
    PaymentPaused { payer: PlayerSlot },
    Ended { winner: PlayerSlot },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PlayerSnapshot {
    pub slot: PlayerSlot,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub health: u16,
    pub alive: bool,
    pub ammo: u32,
    pub reserve: u32,
    pub reloading: bool,
    pub last_input_sequence: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorldSnapshot {
    pub match_id: u128,
    pub server_tick: u64,
    pub phase: MatchPhase,
    pub players: [PlayerSnapshot; 2],
    pub latest_settlement_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlayerBinding {
    pub name: String,
    /// FNN v0.9.0-rc7 node identity: a compressed secp256k1 public key encoded as
    /// 66 lowercase hexadecimal characters without a `0x` prefix.
    pub fiber_pubkey: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldInvoiceTerm {
    pub reservation_id: u16,
    pub payer: PlayerSlot,
    pub payee: PlayerSlot,
    pub amount: u128,
    pub payment_hash: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchTerms {
    pub match_id: u128,
    pub amount_per_damage_bucket: u128,
    pub damage_bucket: u16,
    pub max_total_per_player: u128,
    pub payment_deadline_ms: u64,
    pub invoice_expiry_seconds: u64,
    pub hold_payment_timeout_seconds: u64,
    pub final_expiry_delta_ms: u64,
    pub server_verifying_key: [u8; 32],
    pub players: [PlayerBinding; 2],
    pub hold_invoices: Vec<HoldInvoiceTerm>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SettlementReason {
    Damage { amount: u16 },
    Forfeit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnsignedSettlementIntent {
    pub match_id: u128,
    pub sequence: u64,
    pub reservation_id: u16,
    pub game_tick: u64,
    pub payer: PlayerSlot,
    pub payee: PlayerSlot,
    pub amount: u128,
    pub payment_hash: [u8; 32],
    pub reason: SettlementReason,
    pub state_hash: [u8; 32],
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettlementIntent {
    pub body: UnsignedSettlementIntent,
    pub signature: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum IntentVerificationError {
    #[error("invalid server verifying key")]
    InvalidKey,
    #[error("invalid settlement signature encoding")]
    InvalidSignatureEncoding,
    #[error("settlement signature verification failed")]
    InvalidSignature,
    #[error("settlement intent expired")]
    Expired,
}

impl SettlementIntent {
    pub fn sign(body: UnsignedSettlementIntent, key: &SigningKey) -> Self {
        let message = postcard::to_stdvec(&body).expect("settlement intent is serializable");
        let signature = key.sign(&message).to_bytes().to_vec();
        Self { body, signature }
    }

    pub fn verify(
        &self,
        server_key: &[u8; 32],
        now_ms: u64,
    ) -> Result<(), IntentVerificationError> {
        if now_ms > self.body.expires_at_ms {
            return Err(IntentVerificationError::Expired);
        }
        let key = VerifyingKey::from_bytes(server_key)
            .map_err(|_| IntentVerificationError::InvalidKey)?;
        let signature = Signature::try_from(self.signature.as_slice())
            .map_err(|_| IntentVerificationError::InvalidSignatureEncoding)?;
        let message = postcard::to_stdvec(&self.body).expect("settlement intent is serializable");
        key.verify(&message, &signature)
            .map_err(|_| IntentVerificationError::InvalidSignature)
    }

    pub fn record_hash(&self) -> [u8; 32] {
        *blake3::hash(&postcard::to_stdvec(self).expect("settlement intent is serializable"))
            .as_bytes()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldInvoiceOffer {
    pub match_id: u128,
    pub reservation_id: u16,
    pub payment_hash: [u8; 32],
    pub invoice: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HoldInvoiceStage {
    Funded,
    Received,
    Settled,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldInvoiceAck {
    pub match_id: u128,
    pub reservation_id: u16,
    pub payment_hash: [u8; 32],
    pub stage: HoldInvoiceStage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldInvoiceFailure {
    pub match_id: u128,
    pub reservation_id: u16,
    pub payment_hash: [u8; 32],
    pub stage: HoldInvoiceStage,
    pub error: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldInvoiceRelease {
    pub intent: SettlementIntent,
    pub payment_preimage: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ClientMessage {
    Input(InputFrame),
    AcceptTerms { match_id: u128 },
    HoldInvoiceOffer(HoldInvoiceOffer),
    HoldInvoiceAck(HoldInvoiceAck),
    HoldInvoiceFailure(HoldInvoiceFailure),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ServerMessage {
    Welcome {
        slot: PlayerSlot,
        terms: MatchTerms,
    },
    HoldInvoiceOffer(HoldInvoiceOffer),
    MatchStarted {
        match_id: u128,
    },
    Snapshot(WorldSnapshot),
    HoldInvoiceRelease(HoldInvoiceRelease),
    CancelHoldInvoice {
        match_id: u128,
        reservation_id: u16,
        payment_hash: [u8; 32],
    },
    MatchEnded {
        match_id: u128,
        winner: PlayerSlot,
    },
    Error(String),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_stdvec(message)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> UnsignedSettlementIntent {
        UnsignedSettlementIntent {
            match_id: 9,
            sequence: 3,
            reservation_id: 2,
            game_tick: 64,
            payer: PlayerSlot::B,
            payee: PlayerSlot::A,
            amount: 100,
            payment_hash: [6; 32],
            reason: SettlementReason::Damage { amount: 25 },
            state_hash: [4; 32],
            expires_at_ms: 20_000,
        }
    }

    #[test]
    fn signed_intent_round_trips_and_verifies() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let intent = SettlementIntent::sign(body(), &key);
        let encoded = encode(&intent).unwrap();
        let decoded: SettlementIntent = decode(&encoded).unwrap();

        decoded
            .verify(&key.verifying_key().to_bytes(), 10_000)
            .unwrap();
    }

    #[test]
    fn tampered_intent_is_rejected() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut intent = SettlementIntent::sign(body(), &key);
        intent.body.amount += 1;

        assert!(matches!(
            intent.verify(&key.verifying_key().to_bytes(), 10_000),
            Err(IntentVerificationError::InvalidSignature)
        ));
    }

    #[test]
    fn input_is_sanitized() {
        let input = InputFrame {
            move_x: 5.0,
            move_y: f32::NAN,
            yaw: f32::INFINITY,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(input.move_x, 1.0);
        assert_eq!(input.move_y, 0.0);
        assert_eq!(input.yaw, 0.0);
    }
}
