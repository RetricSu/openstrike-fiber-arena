#!/usr/bin/env bash
set -euo pipefail

# The assertions below grep log files for plain text such as "stage=Settled".
# tracing-subscriber emits ANSI color codes even when writing to files, which
# splits field names from values and breaks those greps, so disable colors.
export NO_COLOR=1

root="$(cd "$(dirname "$0")/.." && pwd)"
port="${1:-5020}"
run_dir="$(mktemp -d "$root/work/secure-desktop.XXXXXX")"

cd "$root"
cargo build --all-features --bins

./target/debug/arena-admin keygen --out-dir "$run_dir/keys"
./target/debug/arena-admin issue-token \
  --netcode-key "$run_dir/keys/netcode.key" \
  --server "127.0.0.1:$port" --name alice --slot a \
  --fiber-pubkey 0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798 \
  --output "$run_dir/alice.token"
./target/debug/arena-admin issue-token \
  --netcode-key "$run_dir/keys/netcode.key" \
  --server "127.0.0.1:$port" --name bob --slot b \
  --fiber-pubkey 02c6047f9441ed7d6d3045406e95c07cd85a778e4b8cef3ca7abac09b95c709ee5 \
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

rg -q "player identity bound.*slot=A.*name=alice" "$run_dir/server.log"
rg -q "player identity bound.*slot=B.*name=bob" "$run_dir/server.log"
rg -q "authoritative damage attacker=A victim=B" "$run_dir/server.log"
rg -q "authoritative damage attacker=B victim=A" "$run_dir/server.log"
rg -q "releasing Fiber hold-invoice preimage" "$run_dir/server.log"
rg -q "stage=Settled" "$run_dir/server.log"
rg -q "match ended" "$run_dir/server.log"
rg -q "adapter:" "$run_dir/alice.log"
rg -q "joined bound match" "$run_dir/alice.log" "$run_dir/bob.log"

echo "Secure desktop smoke test passed. Logs: $run_dir"
