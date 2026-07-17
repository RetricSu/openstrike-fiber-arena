use std::time::{Duration, SystemTime};

use renet_netcode::NETCODE_USER_DATA_BYTES;

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
}
