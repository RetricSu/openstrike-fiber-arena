use std::{
    fs::{self, OpenOptions},
    io::{Cursor, Write},
    net::SocketAddr,
    path::Path,
};

use anyhow::{Context, Result, bail};
use renet_netcode::{ClientAuthentication, ConnectToken};

use crate::{PROTOCOL_ID, net::encode_player_name, net::unix_time};

pub fn load_secret_32(path: &Path, label: &str) -> Result<[u8; 32]> {
    ensure_private_permissions(path, label)?;
    let contents = fs::read(path).with_context(|| format!("reading {label} {}", path.display()))?;
    parse_secret_32(&contents, label)
}

pub fn read_connect_token(path: &Path) -> Result<ConnectToken> {
    ensure_private_permissions(path, "connect token")?;
    let bytes =
        fs::read(path).with_context(|| format!("reading connect token {}", path.display()))?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let token = ConnectToken::read(&mut cursor).context("decoding connect token")?;
    if cursor.position() != bytes.len() as u64 {
        bail!("connect token contains trailing data");
    }
    if token.protocol_id != PROTOCOL_ID {
        bail!("connect token targets a different protocol");
    }
    if token.expire_timestamp <= unix_time().as_secs() {
        bail!("connect token has expired");
    }
    Ok(token)
}

pub fn client_authentication(
    connect_token: Option<&Path>,
    dev_unsecure: bool,
    server_addr: SocketAddr,
    client_id: u64,
    player_name: &str,
) -> Result<ClientAuthentication> {
    match (connect_token, dev_unsecure) {
        (Some(path), false) => Ok(ClientAuthentication::Secure {
            connect_token: read_connect_token(path)?,
        }),
        (None, true) => Ok(ClientAuthentication::Unsecure {
            server_addr,
            client_id,
            user_data: Some(encode_player_name(player_name)),
            protocol_id: PROTOCOL_ID,
        }),
        (Some(_), true) => bail!("--connect-token conflicts with --dev-unsecure"),
        (None, false) => bail!(
            "secure connection required: pass --connect-token, or explicitly use --dev-unsecure"
        ),
    }
}

pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating private file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing private file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing private file {}", path.display()))?;
    Ok(())
}

pub fn write_public_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating public file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing public file {}", path.display()))?;
    Ok(())
}

fn parse_secret_32(contents: &[u8], label: &str) -> Result<[u8; 32]> {
    if let Ok(raw) = <[u8; 32]>::try_from(contents) {
        return Ok(raw);
    }
    let text = std::str::from_utf8(contents)
        .with_context(|| format!("{label} must be 32 raw bytes or hex text"))?;
    let decoded = hex::decode(text.trim().trim_start_matches("0x"))
        .with_context(|| format!("{label} must be valid hex"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} must contain exactly 32 bytes"))
}

#[cfg(unix)]
fn ensure_private_permissions(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .with_context(|| format!("reading metadata for {label} {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{label} {} is accessible by group/others; run chmod 600",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_permissions(_path: &Path, _label: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_raw_and_hex_secrets() {
        assert_eq!(parse_secret_32(&[7; 32], "key").unwrap(), [7; 32]);
        assert_eq!(
            parse_secret_32(format!("0x{}\n", "ab".repeat(32)).as_bytes(), "key").unwrap(),
            [0xab; 32]
        );
    }

    #[test]
    fn rejects_wrong_secret_length() {
        assert!(parse_secret_32(b"abcd", "key").is_err());
    }
}
