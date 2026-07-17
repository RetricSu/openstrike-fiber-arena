use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use openstrike_fiber_arena::{
    PROTOCOL_ID,
    net::{encode_player_name, unix_time},
    security::{load_secret_32, write_private_file, write_public_file},
};
use rand_core::{OsRng, RngCore};
use renet_netcode::ConnectToken;

#[derive(Debug, Parser)]
#[command(about = "Generate Arena server keys and short-lived Renet connect tokens")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate independent Netcode and Ed25519 server secrets.
    Keygen {
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Issue one short-lived bearer token after matchmaking authorization.
    IssueToken {
        #[arg(long)]
        netcode_key: PathBuf,
        #[arg(long)]
        server: SocketAddr,
        #[arg(long)]
        name: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        client_id: Option<u64>,
        #[arg(long, default_value_t = 300)]
        expire_seconds: u64,
        #[arg(long, default_value_t = 15)]
        timeout_seconds: i32,
    },
}

fn main() -> Result<()> {
    match Args::parse().command {
        Command::Keygen { out_dir } => keygen(out_dir),
        Command::IssueToken {
            netcode_key,
            server,
            name,
            output,
            client_id,
            expire_seconds,
            timeout_seconds,
        } => issue_token(
            netcode_key,
            server,
            name,
            output,
            client_id,
            expire_seconds,
            timeout_seconds,
        ),
    }
}

fn keygen(out_dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating key directory {}", out_dir.display()))?;
    for path in [
        out_dir.join("netcode.key"),
        out_dir.join("signing.key"),
        out_dir.join("signing-public.hex"),
    ] {
        ensure!(!path.exists(), "refusing to overwrite {}", path.display());
    }
    let mut netcode_key = [0u8; 32];
    let mut signing_seed = [0u8; 32];
    OsRng.fill_bytes(&mut netcode_key);
    OsRng.fill_bytes(&mut signing_seed);
    let signing_key = SigningKey::from_bytes(&signing_seed);

    write_private_file(
        &out_dir.join("netcode.key"),
        format!("{}\n", hex::encode(netcode_key)).as_bytes(),
    )?;
    write_private_file(
        &out_dir.join("signing.key"),
        format!("{}\n", hex::encode(signing_seed)).as_bytes(),
    )?;
    write_public_file(
        &out_dir.join("signing-public.hex"),
        format!("{}\n", hex::encode(signing_key.verifying_key().to_bytes())).as_bytes(),
    )?;
    println!("generated server keys in {}", out_dir.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn issue_token(
    netcode_key: PathBuf,
    server: SocketAddr,
    name: String,
    output: PathBuf,
    client_id: Option<u64>,
    expire_seconds: u64,
    timeout_seconds: i32,
) -> Result<()> {
    ensure!(expire_seconds > 0, "--expire-seconds must be positive");
    ensure!(
        expire_seconds <= 3_600,
        "--expire-seconds must not exceed one hour"
    );
    ensure!(timeout_seconds > 0, "--timeout-seconds must be positive");
    ensure!(
        !server.ip().is_unspecified() && server.port() != 0,
        "--server must be a reachable public address"
    );
    let key = load_secret_32(&netcode_key, "Netcode private key")?;
    let client_id = client_id.unwrap_or_else(|| {
        loop {
            let value = OsRng.next_u64();
            if value != 0 {
                break value;
            }
        }
    });
    let user_data = encode_player_name(&name);
    let token = ConnectToken::generate(
        unix_time(),
        PROTOCOL_ID,
        expire_seconds,
        client_id,
        timeout_seconds,
        vec![server],
        Some(&user_data),
        &key,
    )
    .context("generating connect token")?;
    let mut bytes = Vec::new();
    token.write(&mut bytes).context("encoding connect token")?;
    write_private_file(&output, &bytes)?;
    println!(
        "issued token for {name} (client {client_id}) to {}",
        output.display()
    );
    Ok(())
}
