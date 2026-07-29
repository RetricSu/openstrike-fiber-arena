#!/usr/bin/env bash
set -euo pipefail

# The assertions below grep log files for plain text such as "stage=Settled".
# tracing-subscriber emits ANSI color codes even when writing to files, which
# splits field names from values and breaks those greps, so disable colors.
export NO_COLOR=1

root="$(cd "$(dirname "$0")/.." && pwd)"
port="${1:-5010}"
log_dir="$root/work/smoke"

cd "$root"
mkdir -p "$log_dir"
cargo build --bins

RUST_LOG=info ./target/debug/arena-server --bind "127.0.0.1:$port" \
  --dev-unsecure --dev-signing-key \
  >"$log_dir/server.log" 2>&1 &
server_pid=$!

cleanup() {
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT

sleep 0.5
RUST_LOG=info ./target/debug/arena-client \
  --server "127.0.0.1:$port" --name alice \
  --dev-unsecure --mock-payments --auto-fire --exit-on-end \
  >"$log_dir/alice.log" 2>&1 &
alice_pid=$!

RUST_LOG=info ./target/debug/arena-client \
  --server "127.0.0.1:$port" --name bob \
  --dev-unsecure --mock-payments --auto-fire --exit-on-end \
  >"$log_dir/bob.log" 2>&1 &
bob_pid=$!

wait "$alice_pid"
wait "$bob_pid"
sleep 0.3

rg -q "all hold invoices received; match started" "$log_dir/server.log"
rg -q "releasing Fiber hold-invoice preimage" "$log_dir/server.log"
rg -q "stage=Settled" "$log_dir/server.log"
rg -q "match ended" "$log_dir/server.log"

echo "Smoke test passed. Logs: $log_dir"
