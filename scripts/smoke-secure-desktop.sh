#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
port="${1:-5020}"
run_dir="$(mktemp -d "$root/work/secure-desktop.XXXXXX")"

cd "$root"
cargo build --all-features --bins

./target/debug/arena-admin keygen --out-dir "$run_dir/keys"
./target/debug/arena-admin issue-token \
  --netcode-key "$run_dir/keys/netcode.key" \
  --server "127.0.0.1:$port" --name alice \
  --output "$run_dir/alice.token"
./target/debug/arena-admin issue-token \
  --netcode-key "$run_dir/keys/netcode.key" \
  --server "127.0.0.1:$port" --name bob \
  --output "$run_dir/bob.token"

RUST_LOG=info ./target/debug/arena-server \
  --bind "127.0.0.1:$port" --public-addr "127.0.0.1:$port" \
  --netcode-key "$run_dir/keys/netcode.key" \
  --signing-key-file "$run_dir/keys/signing.key" \
  --dev-arena >"$run_dir/server.log" 2>&1 &
server_pid=$!

cleanup() {
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT

sleep 0.5
RUST_LOG=info ./target/debug/arena-desktop \
  --server "127.0.0.1:$port" --name alice \
  --connect-token "$run_dir/alice.token" --dev-arena \
  --mock-payments --auto-fire --exit-on-end --width 800 --height 450 \
  >"$run_dir/alice.log" 2>&1 &
alice_pid=$!

RUST_LOG=info ./target/debug/arena-desktop \
  --server "127.0.0.1:$port" --name bob \
  --connect-token "$run_dir/bob.token" --dev-arena \
  --mock-payments --auto-fire --exit-on-end --width 800 --height 450 \
  >"$run_dir/bob.log" 2>&1 &
bob_pid=$!

wait "$alice_pid"
wait "$bob_pid"
sleep 0.3

rg -q "player connected.*name=alice" "$run_dir/server.log"
rg -q "player connected.*name=bob" "$run_dir/server.log"
rg -q "authoritative damage attacker=A victim=B" "$run_dir/server.log"
rg -q "authoritative damage attacker=B victim=A" "$run_dir/server.log"
rg -q "issuing Fiber settlement intent" "$run_dir/server.log"
rg -q "settlement acknowledgement" "$run_dir/server.log"
rg -q "match ended" "$run_dir/server.log"
rg -q "adapter:" "$run_dir/alice.log"
rg -q "payment #[0-9]+ completed: Success" "$run_dir/alice.log" "$run_dir/bob.log"

echo "Secure desktop smoke test passed. Logs: $run_dir"
