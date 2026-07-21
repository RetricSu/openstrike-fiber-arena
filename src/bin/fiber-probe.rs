use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use openstrike_fiber_arena::fiber::FiberRpcClient;

#[derive(Debug, Parser)]
#[command(about = "Check an FNN node and its direct game-payment channel")]
struct Args {
    #[arg(long, env = "FIBER_RPC_URL", default_value = "http://127.0.0.1:8227")]
    fiber_rpc: String,
    #[arg(long, env = "FIBER_PEER_PUBKEY", required_unless_present = "node_only")]
    peer_pubkey: Option<String>,
    /// Required outbound liquidity in shannons. The default matches the arena
    /// server's default per-player match cap.
    #[arg(long, default_value_t = 100_000)]
    required_outbound: u128,
    #[arg(long, default_value_t = 2_000)]
    payment_timeout_ms: u64,
    /// Only verify RPC reachability and print local node identity.
    #[arg(long, default_value_t = false)]
    node_only: bool,
    /// Skip FNN's non-committing payment route check.
    #[arg(long, default_value_t = false)]
    skip_dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let rpc = FiberRpcClient::new(&args.fiber_rpc);
    let node = rpc
        .node_info()
        .await
        .with_context(|| format!("probing Fiber RPC at {}", args.fiber_rpc))?;
    println!(
        "FNN {} ready: pubkey={} chain={}",
        node.version, node.pubkey, node.chain_hash
    );

    if args.node_only {
        return Ok(());
    }

    let target = args
        .peer_pubkey
        .as_deref()
        .expect("clap requires peer pubkey unless node-only");
    let readiness = rpc
        .check_direct_channel(target, args.required_outbound)
        .await
        .context("checking direct Fiber channel")?;
    println!(
        "direct channel ready: id={} outbound={} remote={} one_way={}",
        readiness.channel.channel_id,
        readiness.channel.local_balance,
        readiness.channel.remote_balance,
        readiness.channel.is_one_way
    );

    if !args.skip_dry_run {
        rpc.dry_run_keysend(
            target,
            args.required_outbound.max(1),
            Duration::from_millis(args.payment_timeout_ms),
        )
        .await
        .context("dry-running the Fiber keysend route")?;
        println!("keysend dry run succeeded");
    }

    Ok(())
}
