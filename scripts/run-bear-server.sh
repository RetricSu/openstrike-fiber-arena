#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

bear_host="${BEAR_HOST:-bear}"
remote_dir="${REMOTE_DIR:-/home/retric/openstrike-fiber-arena}"
port="${PORT:-5000}"
bind_addr="${BIND_ADDR:-0.0.0.0}"
public_addr="${PUBLIC_ADDR:-}"
players_text="${PLAYERS:-alice bob}"
expire_seconds="${EXPIRE_SECONDS:-3600}"
tmux_session="${TMUX_SESSION:-ai}"
tmux_window="${TMUX_WINDOW:-openstrike-arena}"
build_profile="${BUILD_PROFILE:-debug}"
restart="${RESTART:-1}"
rust_log="${ARENA_RUST_LOG:-info}"
local_state_dir="${LOCAL_STATE_DIR:-$root/work/bear-server}"
map_path="${MAP_PATH:-}"
wad_dirs=()
if [ -n "${WAD_DIR:-}" ]; then
  wad_dirs+=("$WAD_DIR")
fi
cargo_http_timeout="${CARGO_HTTP_TIMEOUT:-120}"
cargo_net_retry="${CARGO_NET_RETRY:-10}"
cargo_registry_protocol="${CARGO_REGISTRIES_CRATES_IO_PROTOCOL:-sparse}"

usage() {
  cat <<'USAGE'
Usage: scripts/run-bear-server.sh [options]

Deploy the arena server to bear, build it, issue short-lived connect tokens,
and run the authoritative server in the bear tmux session.

Options:
  --host HOST             SSH host, default: bear
  --remote-dir DIR        Remote project directory
  --port PORT             UDP game port, default: 5000
  --public-addr ADDR      Reachable IP:port for client tokens
  --players "A B"         Space-separated player names, default: "alice bob"
  --expire-seconds SEC    Connect token TTL, default: 3600
  --rust-log FILTER       Remote arena-server log filter, default: info
  --map PATH              Sync and run a GoldSrc BSP instead of --dev-arena
  --wad-dir DIR           Sync a WAD search directory (repeatable)
  --release               Build and run release binaries
  --no-restart            Refuse to replace an existing tmux window
  -h, --help              Show this help

Environment variables with the same uppercase names are also supported:
BEAR_HOST, REMOTE_DIR, PORT, PUBLIC_ADDR, PLAYERS, EXPIRE_SECONDS,
TMUX_SESSION, TMUX_WINDOW, BUILD_PROFILE, RESTART, ARENA_RUST_LOG, LOCAL_STATE_DIR,
MAP_PATH, WAD_DIR,
CARGO_HTTP_TIMEOUT, CARGO_NET_RETRY, CARGO_REGISTRIES_CRATES_IO_PROTOCOL.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --host)
      bear_host="$2"
      shift 2
      ;;
    --remote-dir)
      remote_dir="$2"
      shift 2
      ;;
    --port)
      port="$2"
      shift 2
      ;;
    --public-addr)
      public_addr="$2"
      shift 2
      ;;
    --players)
      players_text="$2"
      shift 2
      ;;
    --expire-seconds)
      expire_seconds="$2"
      shift 2
      ;;
    --rust-log)
      rust_log="$2"
      shift 2
      ;;
    --map)
      map_path="$2"
      shift 2
      ;;
    --wad-dir)
      wad_dirs+=("$2")
      shift 2
      ;;
    --release)
      build_profile="release"
      shift
      ;;
    --no-restart)
      restart="0"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$port" in
  *[!0-9]* | "")
    echo "--port must be a number" >&2
    exit 2
    ;;
esac

case "$expire_seconds" in
  *[!0-9]* | "")
    echo "--expire-seconds must be a number" >&2
    exit 2
    ;;
esac

if [ "$build_profile" != "debug" ] && [ "$build_profile" != "release" ]; then
  echo "BUILD_PROFILE must be debug or release" >&2
  exit 2
fi

sq() {
  printf "'%s'" "$(printf "%s" "$1" | sed "s/'/'\\\\''/g")"
}

remote_bash() {
  ssh "$bear_host" "bash -lc $(sq "$1")"
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing local command: $1" >&2
    exit 1
  fi
}

require_cmd rsync
require_cmd ssh
require_cmd scp

if [ -n "$map_path" ]; then
  if [ ! -f "$map_path" ]; then
    echo "map not found: $map_path" >&2
    exit 1
  fi
  map_path="$(cd "$(dirname "$map_path")" && pwd)/$(basename "$map_path")"
  for i in "${!wad_dirs[@]}"; do
    if [ ! -d "${wad_dirs[$i]}" ]; then
      echo "WAD directory not found: ${wad_dirs[$i]}" >&2
      exit 1
    fi
    wad_dirs[$i]="$(cd "${wad_dirs[$i]}" && pwd)"
  done
elif [ "${#wad_dirs[@]}" -gt 0 ]; then
  echo "--wad-dir requires --map" >&2
  exit 2
fi

IFS=' ' read -r -a players <<<"$players_text"
if [ "${#players[@]}" -eq 0 ]; then
  echo "at least one player name is required" >&2
  exit 2
fi
for player in "${players[@]}"; do
  case "$player" in
    "" | *[!A-Za-z0-9_-]*)
      echo "player names may only contain letters, numbers, underscore, and dash: $player" >&2
      exit 2
      ;;
  esac
done

if [ -z "$public_addr" ]; then
  detected_ip="$(
    remote_bash "ip -4 -o addr show tailscale0 2>/dev/null | awk '{print \$4}' | cut -d/ -f1 | head -n1 || true"
  )"
  if [ -z "$detected_ip" ]; then
    detected_ip="$(
      remote_bash "hostname -I 2>/dev/null | awk '{print \$1}' || true"
    )"
  fi
  if [ -z "$detected_ip" ]; then
    echo "could not auto-detect bear public address; pass --public-addr IP:PORT" >&2
    exit 1
  fi
  public_addr="$detected_ip:$port"
fi

if [[ "$public_addr" != *:* ]]; then
  public_addr="$public_addr:$port"
fi

bin_dir="debug"
cargo_env="CARGO_HTTP_TIMEOUT=$(sq "$cargo_http_timeout") CARGO_NET_RETRY=$(sq "$cargo_net_retry") CARGO_REGISTRIES_CRATES_IO_PROTOCOL=$(sq "$cargo_registry_protocol")"
cargo_build="$cargo_env cargo build --features openstrike --bin arena-server --bin arena-admin"
if [ "$build_profile" = "release" ]; then
  bin_dir="release"
  cargo_build="$cargo_build --release"
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
local_token_dir="$local_state_dir/$timestamp"
mkdir -p "$local_token_dir"

echo "Deploying to $bear_host:$remote_dir"
echo "Public game address: $public_addr"
echo "Players: ${players[*]}"
if [ -n "$map_path" ]; then
  echo "Map: $map_path"
fi

remote_bash "command -v rsync >/dev/null && command -v tmux >/dev/null && command -v cargo >/dev/null"
remote_bash "mkdir -p $(sq "$remote_dir")"

rsync -az \
  --exclude '/target/' \
  --exclude '/work/' \
  --exclude '/outputs/' \
  --exclude '/.git/' \
  --exclude '/secrets/' \
  --exclude '/run/' \
  "$root/" "$bear_host:$remote_dir/"

server_map_args="--dev-arena"
client_map_args="--dev-arena"
if [ -n "$map_path" ]; then
  remote_asset_dir="$remote_dir/run/assets/$timestamp"
  remote_map="$remote_asset_dir/maps/$(basename "$map_path")"
  remote_bash "mkdir -p $(sq "$remote_asset_dir/maps")"
  rsync -az "$map_path" "$bear_host:$remote_map"

  server_map_args="--map $(sq "$remote_map")"
  client_map_args="--map $(sq "$map_path")"
  for i in "${!wad_dirs[@]}"; do
    remote_wad_dir="$remote_asset_dir/wads/$i"
    remote_bash "mkdir -p $(sq "$remote_wad_dir")"
    rsync -az "${wad_dirs[$i]}/" "$bear_host:$remote_wad_dir/"
    server_map_args="$server_map_args --wad-dir $(sq "$remote_wad_dir")"
    client_map_args="$client_map_args --wad-dir $(sq "${wad_dirs[$i]}")"
  done
fi

remote_bash "cd $(sq "$remote_dir") && $cargo_build"

remote_bash "
set -euo pipefail
cd $(sq "$remote_dir")
mkdir -p secrets run/tokens logs
if [ -f secrets/netcode.key ] && [ -f secrets/signing.key ]; then
  chmod 600 secrets/netcode.key secrets/signing.key
elif [ ! -e secrets/netcode.key ] && [ ! -e secrets/signing.key ]; then
  ./target/$bin_dir/arena-admin keygen --out-dir secrets
else
  echo 'partial server key state in secrets/; fix it manually before continuing' >&2
  exit 1
fi
rm -f run/tokens/*.token
"

for player in "${players[@]}"; do
  remote_bash "
set -euo pipefail
cd $(sq "$remote_dir")
./target/$bin_dir/arena-admin issue-token \
  --netcode-key secrets/netcode.key \
  --server $(sq "$public_addr") \
  --name $(sq "$player") \
  --output $(sq "run/tokens/$player.token") \
  --expire-seconds $(sq "$expire_seconds")
"
  scp "$bear_host:$remote_dir/run/tokens/$player.token" "$local_token_dir/$player.token" >/dev/null
  chmod 600 "$local_token_dir/$player.token"
done

server_cmd="cd $(sq "$remote_dir") && RUST_LOG=$(sq "$rust_log") ./target/$bin_dir/arena-server --bind $(sq "$bind_addr:$port") --public-addr $(sq "$public_addr") --netcode-key secrets/netcode.key --signing-key-file secrets/signing.key $server_map_args 2>&1 | tee -a $(sq "logs/server-$port.log")"

remote_bash "
set -euo pipefail
tmux has-session -t $(sq "$tmux_session") 2>/dev/null || tmux new-session -d -s $(sq "$tmux_session") -c $(sq "$remote_dir")
if tmux list-windows -t $(sq "$tmux_session") -F '#W' | grep -Fxq $(sq "$tmux_window"); then
  if [ $(sq "$restart") = '1' ]; then
    tmux kill-window -t $(sq "$tmux_session:$tmux_window")
  else
    echo 'tmux window already exists; pass RESTART=1 or omit --no-restart' >&2
    exit 1
  fi
fi
tmux new-window -t $(sq "$tmux_session") -n $(sq "$tmux_window") -c $(sq "$remote_dir")
tmux send-keys -t $(sq "$tmux_session:$tmux_window") $(sq "$server_cmd") C-m
"

sleep 1
remote_bash "tmux capture-pane -t $(sq "$tmux_session:$tmux_window") -p -S -40"

latest_link="$local_state_dir/latest"
rm -f "$latest_link"
ln -s "$local_token_dir" "$latest_link"

echo
echo "Bear arena server is running at $public_addr"
echo "Local tokens copied to $local_token_dir"
echo "Latest token symlink: $latest_link"
echo
echo "Client examples:"
for player in "${players[@]}"; do
  echo "cargo run --features desktop --bin arena-desktop -- --server $public_addr --name $player --connect-token $latest_link/$player.token $client_map_args --mock-payments"
done
echo
echo "Remote logs:"
echo "ssh $bear_host 'tmux capture-pane -t $tmux_session:$tmux_window -p -S -120'"
