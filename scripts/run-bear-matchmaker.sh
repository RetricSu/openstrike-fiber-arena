#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bear_host="${BEAR_HOST:-bear}"
remote_dir="${REMOTE_DIR:-/home/retric/openstrike-fiber-arena}"
http_bind="${HTTP_BIND:-127.0.0.1:8080}"
public_http_url="${PUBLIC_HTTP_URL:-}"
tmux_session="${TMUX_SESSION:-ai}"
tmux_window="${TMUX_WINDOW:-openstrike-matchmaker}"
build_profile="${BUILD_PROFILE:-release}"
restart="${RESTART:-1}"
rust_log="${MATCHMAKER_RUST_LOG:-info}"
game_rust_log="${ARENA_RUST_LOG:-info}"
map_path="${MAP_PATH:-}"
wad_dirs=()
game_endpoints=()

usage() {
  cat <<'USAGE'
Usage: scripts/run-bear-matchmaker.sh [options]

Deploy the HTTP room service and its per-room arena-server worker binary to
bear. UDP tunnels must already map every local port to the corresponding
public host:port supplied here.

Options:
  --game-endpoint LOCAL=PUBLIC  Repeat for every room slot, for example:
                                --game-endpoint 5100=147.185.221.1:30001
  --host HOST                    SSH host, default: bear
  --remote-dir DIR               Remote project directory
  --http-bind IP:PORT            Local HTTP listener, default: 127.0.0.1:8080
  --public-http-url URL           Matchmaker URL printed in client examples
  --map PATH                     Sync and use a GoldSrc BSP instead of neon
  --wad-dir DIR                  Sync a WAD search directory (repeatable)
  --debug                        Build debug binaries instead of release
  --no-restart                   Refuse to replace the existing tmux window
  -h, --help                     Show this help

Environment: BEAR_HOST, REMOTE_DIR, HTTP_BIND, PUBLIC_HTTP_URL, TMUX_SESSION,
TMUX_WINDOW, BUILD_PROFILE, RESTART, MATCHMAKER_RUST_LOG, ARENA_RUST_LOG,
MAP_PATH, WAD_DIR.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --game-endpoint)
      game_endpoints+=("$2")
      shift 2
      ;;
    --host)
      bear_host="$2"
      shift 2
      ;;
    --remote-dir)
      remote_dir="$2"
      shift 2
      ;;
    --http-bind)
      http_bind="$2"
      shift 2
      ;;
    --public-http-url)
      public_http_url="$2"
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
    --debug)
      build_profile="debug"
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

if [ "${#game_endpoints[@]}" -eq 0 ]; then
  echo "at least one --game-endpoint is required" >&2
  exit 2
fi
if [ "$build_profile" != "debug" ] && [ "$build_profile" != "release" ]; then
  echo "BUILD_PROFILE must be debug or release" >&2
  exit 2
fi
if [ -n "${WAD_DIR:-}" ]; then
  wad_dirs+=("$WAD_DIR")
fi
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
    wad_dirs[i]="$(cd "${wad_dirs[i]}" && pwd)"
  done
elif [ "${#wad_dirs[@]}" -gt 0 ]; then
  echo "--wad-dir requires --map" >&2
  exit 2
fi

sq() {
  printf "'%s'" "$(printf "%s" "$1" | sed "s/'/'\\\\''/g")"
}

remote_bash() {
  # shellcheck disable=SC2029
  ssh "$bear_host" "bash -lc $(sq "$1")"
}

for command in rsync ssh; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "missing local command: $command" >&2
    exit 1
  fi
done

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
bin_dir="$build_profile"
cargo_build="cargo build --features openstrike --bin arena-matchmaker --bin arena-server --bin arena-admin"
if [ "$build_profile" = "release" ]; then
  cargo_build="$cargo_build --release"
fi

echo "Deploying HTTP rooms to $bear_host:$remote_dir"
echo "HTTP listener: $http_bind"
printf "UDP endpoints:\n"
printf "  %s\n" "${game_endpoints[@]}"

remote_bash "command -v rsync >/dev/null && command -v tmux >/dev/null && command -v cargo >/dev/null && command -v curl >/dev/null"
remote_bash "mkdir -p $(sq "$remote_dir")"
rsync -az \
  --exclude '/target/' \
  --exclude '/work/' \
  --exclude '/outputs/' \
  --exclude '/.git/' \
  --exclude '/secrets/' \
  --exclude '/run/' \
  "$root/" "$bear_host:$remote_dir/"

remote_map_args=""
if [ -n "$map_path" ]; then
  remote_asset_dir="$remote_dir/run/assets/$timestamp"
  remote_map="$remote_asset_dir/maps/$(basename "$map_path")"
  remote_bash "mkdir -p $(sq "$remote_asset_dir/maps")"
  rsync -az "$map_path" "$bear_host:$remote_map"
  remote_map_args="--map $(sq "$remote_map")"
  for i in "${!wad_dirs[@]}"; do
    remote_wad_dir="$remote_asset_dir/wads/$i"
    remote_bash "mkdir -p $(sq "$remote_wad_dir")"
    rsync -az "${wad_dirs[$i]}/" "$bear_host:$remote_wad_dir/"
    remote_map_args="$remote_map_args --wad-dir $(sq "$remote_wad_dir")"
  done
fi

remote_bash "cd $(sq "$remote_dir") && $cargo_build"
remote_bash "
set -euo pipefail
cd $(sq "$remote_dir")
mkdir -p secrets logs/matches
if [ -f secrets/netcode.key ] && [ -f secrets/signing.key ]; then
  chmod 600 secrets/netcode.key secrets/signing.key
elif [ ! -e secrets/netcode.key ] && [ ! -e secrets/signing.key ]; then
  ./target/$bin_dir/arena-admin keygen --out-dir secrets
else
  echo 'partial server key state in secrets/; fix it manually' >&2
  exit 1
fi
"

endpoint_args=""
for endpoint in "${game_endpoints[@]}"; do
  endpoint_args="$endpoint_args --game-endpoint $(sq "$endpoint")"
done
matchmaker_cmd="cd $(sq "$remote_dir") && exec env RUST_LOG=$(sq "$rust_log") ./target/$bin_dir/arena-matchmaker --http-bind $(sq "$http_bind") $endpoint_args --netcode-key secrets/netcode.key --signing-key-file secrets/signing.key --server-bin ./target/$bin_dir/arena-server --log-dir logs/matches --game-rust-log $(sq "$game_rust_log") $remote_map_args >> $(sq "logs/matchmaker.log") 2>&1"

remote_bash "
set -euo pipefail
tmux has-session -t $(sq "$tmux_session") 2>/dev/null || tmux new-session -d -s $(sq "$tmux_session") -c $(sq "$remote_dir")
if tmux list-windows -t $(sq "$tmux_session") -F '#W' | grep -Fxq $(sq "$tmux_window"); then
  if [ $(sq "$restart") = '1' ]; then
    tmux send-keys -t $(sq "$tmux_session:$tmux_window") C-c
    sleep 1
    tmux kill-window -t $(sq "$tmux_session:$tmux_window") 2>/dev/null || true
  else
    echo 'tmux window already exists; omit --no-restart to replace it' >&2
    exit 1
  fi
fi
tmux new-window -t $(sq "$tmux_session") -n $(sq "$tmux_window") -c $(sq "$remote_dir")
tmux send-keys -t $(sq "$tmux_session:$tmux_window") $(sq "$matchmaker_cmd") C-m
"

remote_bash "
set -euo pipefail
for _ in \$(seq 1 40); do
  if curl -fsS $(sq "http://$http_bind/healthz"); then
    exit 0
  fi
  sleep 0.25
done
echo 'matchmaker did not become healthy' >&2
tail -80 $(sq "$remote_dir/logs/matchmaker.log") >&2
exit 1
"

echo
echo "Matchmaker is healthy on bear at http://$http_bind"
if [ -n "$public_http_url" ]; then
  echo "Public room URL: $public_http_url"
  echo
  echo "Player A creates and prints a room code:"
  echo "cargo run --features desktop --bin arena-desktop -- --name alice --matchmaker $public_http_url --fiber-rpc http://127.0.0.1:8227 --dev-arena"
  echo
  echo "Player B joins that code:"
  echo "cargo run --features desktop --bin arena-desktop -- --name bob --matchmaker $public_http_url --room ROOMCODE --fiber-rpc http://127.0.0.1:8227 --dev-arena"
fi
echo
echo "Logs: ssh $bear_host 'tail -f $remote_dir/logs/matchmaker.log'"
