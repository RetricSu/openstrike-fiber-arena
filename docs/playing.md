# Playing OpenStrike Fiber Arena

This archive contains pre-built binaries for a 1v1 arena shooter whose damage
settles over the [Fiber](https://www.fiber.world/) payment network. You can
play a full local match with mocked payments — no Rust toolchain, no Fiber
node, and no game assets to download.

## Contents

| Binary | Purpose |
| --- | --- |
| `arena-desktop` | The game: native first-person client with HUD. |
| `arena-server` | Authoritative 64 Hz game server; decides all damage. |
| `arena-client` | Headless client — use it as a bot opponent. |
| `arena-admin` | `keygen` / `issue-token` for production deployments. |
| `arena-matchmaker` | HTTP invite-room service for public demos. |
| `fiber-probe` | Checks a local Fiber node before a funded match. |
| `assets/` | The procedural duelist model the client loads at runtime. |

## Quickstart: play against a bot

Open three terminals in the extracted directory (three PowerShell windows on
Windows; append `.exe` to each command):

```sh
# 1 — authoritative server (insecure dev mode, random signing key)
./arena-server --dev-unsecure --dev-signing-key

# 2 — bot opponent with mocked payments
./arena-client --name bot --dev-unsecure --mock-payments --auto-fire --exit-on-end

# 3 — you, in the built-in neon arena
./arena-desktop --name you --dev-unsecure --mock-payments --dev-arena
```

The match starts once both clients are in: WASD to move, mouse to look, left
click to fire, R to reload. Every 25 damage you take settles one mocked
hold invoice; the HUD shows the settlement flow. When the match ends, start
the two clients again for a rematch — the server keeps running.

Platform notes:

- **macOS**: the binaries are unsigned. After extracting, run
  `xattr -dr com.apple.quarantine .` once inside the directory, then launch
  normally.
- **Windows**: SmartScreen may flag the unrecognized app — choose
  "More info → Run anyway".
- **Linux**: `arena-desktop` needs a working GPU driver (Vulkan or OpenGL).
  The server and bot client are headless and run anywhere.

## Two players on a LAN

One machine hosts with its LAN address announced:

```sh
./arena-server --bind 0.0.0.0:5000 --public-addr 192.168.1.10:5000 \
  --dev-unsecure --dev-signing-key
```

Each player then connects a desktop client from their own machine:

```sh
./arena-desktop --name alice --server 192.168.1.10:5000 \
  --dev-unsecure --mock-payments --dev-arena
```

## Real Fiber payments

`--mock-payments` replaces the wallet with a mock adapter. A funded match
needs a local Fiber node (FNN `v0.9.0-rc7`) per player with a direct channel
between them, and production-grade connect tokens instead of `--dev-unsecure`.
That setup is documented in the source repository:

- <https://github.com/RetricSu/openstrike-fiber-arena#readme> — secure
  deployment bootstrap, invite-code rooms, funded matches.
- `docs/development.md` — build from source and the developer quickstart.
- `docs/fiber-integration.md` — the verified FNN flow and trust boundaries.
