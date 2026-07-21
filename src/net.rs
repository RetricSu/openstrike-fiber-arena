use std::time::{Duration, SystemTime};

use renet_netcode::NETCODE_USER_DATA_BYTES;

use crate::protocol::{PlayerBinding, PlayerSlot};

const IDENTITY_MAGIC: &[u8; 4] = b"OFI1";
const MAX_PLAYER_NAME_BYTES: usize = 64;

pub const MOCK_FIBER_PUBKEY_A: &str =
    "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
pub const MOCK_FIBER_PUBKEY_B: &str =
    "02c6047f9441ed7d6d3045406e95c07cd85a778e4b8cef3ca7abac09b95c709ee5";

pub fn unix_time() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
}

pub fn unix_ms() -> u64 {
    unix_time().as_millis().min(u64::MAX as u128) as u64
}

pub fn encode_player_name(name: &str) -> [u8; NETCODE_USER_DATA_BYTES] {
    let mut output = [0u8; NETCODE_USER_DATA_BYTES];
    let bytes = name.as_bytes();
    let length = bytes.len().min(NETCODE_USER_DATA_BYTES - 2);
    output[..2].copy_from_slice(&(length as u16).to_le_bytes());
    output[2..length + 2].copy_from_slice(&bytes[..length]);
    output
}

pub fn decode_player_name(data: &[u8; NETCODE_USER_DATA_BYTES]) -> String {
    let length = u16::from_le_bytes([data[0], data[1]]) as usize;
    let length = length.min(NETCODE_USER_DATA_BYTES - 2);
    String::from_utf8_lossy(&data[2..length + 2]).into_owned()
}

pub fn mock_fiber_pubkey(slot: PlayerSlot) -> &'static str {
    match slot {
        PlayerSlot::A => MOCK_FIBER_PUBKEY_A,
        PlayerSlot::B => MOCK_FIBER_PUBKEY_B,
    }
}

pub fn normalize_fiber_pubkey(value: &str) -> Result<String, String> {
    let value = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    let bytes = hex::decode(value).map_err(|_| "Fiber pubkey must be hexadecimal")?;
    if bytes.len() != 33 {
        return Err("Fiber pubkey must be a 33-byte compressed secp256k1 key".into());
    }
    if !matches!(bytes[0], 0x02 | 0x03) {
        return Err("Fiber pubkey must start with 02 or 03".into());
    }
    Ok(hex::encode(bytes))
}

pub fn validate_player_binding(binding: &PlayerBinding) -> Result<PlayerBinding, String> {
    let name = binding.name.trim();
    if name.is_empty() {
        return Err("player name must not be empty".into());
    }
    if name.len() > MAX_PLAYER_NAME_BYTES {
        return Err(format!(
            "player name exceeds {MAX_PLAYER_NAME_BYTES} UTF-8 bytes"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("player name must not contain control characters".into());
    }
    Ok(PlayerBinding {
        name: name.to_owned(),
        fiber_pubkey: normalize_fiber_pubkey(&binding.fiber_pubkey)?,
    })
}

/// Encodes the matchmaking-authorized identity into Netcode's encrypted user
/// data. The slot and FNN identity pubkey therefore cannot be altered without
/// the server's Netcode key.
pub fn encode_player_identity(
    slot: PlayerSlot,
    binding: &PlayerBinding,
) -> Result<[u8; NETCODE_USER_DATA_BYTES], String> {
    let binding = validate_player_binding(binding)?;
    let payload = postcard::to_stdvec(&(slot, binding)).map_err(|error| error.to_string())?;
    let header_len = IDENTITY_MAGIC.len() + 2;
    if payload.len() > NETCODE_USER_DATA_BYTES - header_len {
        return Err("encoded player identity exceeds Netcode user-data capacity".into());
    }
    let mut output = [0u8; NETCODE_USER_DATA_BYTES];
    output[..IDENTITY_MAGIC.len()].copy_from_slice(IDENTITY_MAGIC);
    output[IDENTITY_MAGIC.len()..header_len].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    output[header_len..header_len + payload.len()].copy_from_slice(&payload);
    Ok(output)
}

pub fn decode_player_identity(
    data: &[u8; NETCODE_USER_DATA_BYTES],
) -> Result<(PlayerSlot, PlayerBinding), String> {
    if &data[..IDENTITY_MAGIC.len()] != IDENTITY_MAGIC {
        return Err("connect token does not contain a v1 player identity".into());
    }
    let header_len = IDENTITY_MAGIC.len() + 2;
    let length =
        u16::from_le_bytes([data[IDENTITY_MAGIC.len()], data[IDENTITY_MAGIC.len() + 1]]) as usize;
    if length == 0 || length > NETCODE_USER_DATA_BYTES - header_len {
        return Err("connect token player identity has an invalid length".into());
    }
    let (slot, binding): (PlayerSlot, PlayerBinding) =
        postcard::from_bytes(&data[header_len..header_len + length])
            .map_err(|error| format!("invalid connect token player identity: {error}"))?;
    Ok((slot, validate_player_binding(&binding)?))
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openstrike_fiber_arena=info".into()),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_name_round_trips() {
        let data = encode_player_name("alice");
        assert_eq!(decode_player_name(&data), "alice");
    }

    #[test]
    fn authenticated_identity_round_trips_and_normalizes_pubkey() {
        let binding = PlayerBinding {
            name: "alice".into(),
            fiber_pubkey: format!("0x{}", MOCK_FIBER_PUBKEY_A.to_uppercase()),
        };
        let data = encode_player_identity(PlayerSlot::A, &binding).unwrap();
        let (slot, decoded) = decode_player_identity(&data).unwrap();
        assert_eq!(slot, PlayerSlot::A);
        assert_eq!(decoded.name, "alice");
        assert_eq!(decoded.fiber_pubkey, MOCK_FIBER_PUBKEY_A);
    }

    #[test]
    fn legacy_name_is_not_accepted_as_an_authenticated_identity() {
        let data = encode_player_name("alice");
        assert!(decode_player_identity(&data).is_err());
    }
}
