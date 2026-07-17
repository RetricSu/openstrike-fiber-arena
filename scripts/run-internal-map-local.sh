#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
map_path="${MAP_PATH:-$root/work/maps/cs16/maps/de_dust2.bsp}"
wad_dir="${WAD_DIR:-$root/work/maps/cs16/support}"
server="${SERVER:-127.0.0.1:5000}"
mode="${1:-}"

usage() {
  cat <<'USAGE'
Usage:
  scripts/run-internal-map-local.sh server
  scripts/run-internal-map-local.sh PLAYER_NAME

Run the server first, then run alice and bob in two more terminals. MAP_PATH,
WAD_DIR, and SERVER can override the internal-test defaults under work/maps/.
Extra arguments are passed to arena-server or arena-desktop.
USAGE
}

if [ -z "$mode" ] || [ "$mode" = "-h" ] || [ "$mode" = "--help" ]; then
  usage
  [ -n "$mode" ] && exit 0
  exit 2
fi
shift

if [ ! -f "$map_path" ]; then
  echo "internal test map not found: $map_path" >&2
  exit 1
fi
if [ ! -d "$wad_dir" ]; then
  echo "internal test WAD directory not found: $wad_dir" >&2
  exit 1
fi

cd "$root"
if [ "$mode" = "server" ]; then
  exec cargo run --features desktop --bin arena-server -- \
    --dev-unsecure --dev-signing-key \
    --bind "$server" --public-addr "$server" \
    --map "$map_path" --wad-dir "$wad_dir" \
    "$@"
fi

exec cargo run --features desktop --bin arena-desktop -- \
  --server "$server" --name "$mode" \
  --dev-unsecure --mock-payments \
  --map "$map_path" --wad-dir "$wad_dir" \
  "$@"
