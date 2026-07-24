# Public invite rooms

The public demo keeps game traffic and matchmaking traffic separate:

```text
                   HTTPS (Cloudflare Tunnel)
desktop client  ------------------------------>  arena-matchmaker
       |                                             |
       | local FNN node_info.pubkey                  | short-lived token
       |                                             | spawn one process
       |                 Renet UDP                   v
       +-------------------------------------->  arena-server
                    (Playit UDP endpoint)       one room / two seats
       |
       +---- JSON-RPC ----> local FNN ---- Fiber channel ---- opponent FNN
```

The matchmaker never calls FNN RPC and never receives a wallet private key. It
only receives the public FNN node identity needed for the immutable match
binding. Hold-invoice creation, validation, payment, settlement, and
cancellation remain in each player's local client and FNN.

## Player flow

1. Player A starts the desktop client with `--matchmaker` and no `--room`.
2. The client reads `node_info.pubkey` from its configured FNN, creates a room,
   prints the eight-character code, and polls its private room ticket.
3. Player A shares the room code with Player B.
4. Player B starts with the same matchmaker URL and `--room CODE`.
5. The service validates both player bindings, assigns A/B, reserves one UDP
   endpoint, starts a dedicated `arena-server`, and issues two independent
   short-lived Netcode tokens.
6. Each client receives only its own bearer token and connects to the public
   UDP endpoint.
7. The normal real-Fiber channel check and hold-invoice handshake must finish
   before the server changes the match phase to `Live`.

`--mock-payments` is deliberately rejected in matchmaker mode. A public room
therefore demonstrates the actual FNN v0.9.0-rc7 flow, not just the game UI.

If a system-wide TUN proxy captures or drops arbitrary UDP, bind the game
socket to a physical-interface address while leaving the port ephemeral:

```sh
arena-desktop ... --local-bind 192.168.1.20:0
```

The default remains `0.0.0.0:0`. This option changes only the client's local
UDP source address; Fiber RPC and HTTPS matchmaking continue to use their
normal routes.

## HTTP API

The HTTP listener should bind to loopback and be published as HTTPS through
Cloudflare Tunnel. Responses that contain room tickets or connect tokens use
`Cache-Control: no-store`. Do not put a caching proxy in front of these paths.

### Create

```http
POST /v1/rooms
Content-Type: application/json

{
  "name": "alice",
  "fiber_pubkey": "02..."
}
```

The `201` response has state `waiting_for_opponent`, slot `A`, the room code,
and a random bearer `ticket`.

### Join

```http
POST /v1/rooms/ABCD2345/join
Content-Type: application/json

{
  "name": "bob",
  "fiber_pubkey": "03..."
}
```

The first valid join receives slot `B`. Another join receives:

```json
{
  "code": "room_full",
  "message": "room already has two players"
}
```

with HTTP status `409`. If every configured UDP endpoint is occupied, a valid
waiting room remains intact and the join gets `503 no_game_endpoint`.

The service also rejects duplicate player names and duplicate Fiber pubkeys
inside a room.

### Poll or leave

```http
GET /v1/rooms/ABCD2345
Authorization: Bearer PRIVATE_TICKET

DELETE /v1/rooms/ABCD2345
Authorization: Bearer PRIVATE_TICKET

GET /healthz
```

Only the random ticket reveals that player's slot, public game address, and
connect token. The ticket is sent in the authorization header so it does not
appear in Cloudflare or reverse-proxy URL logs. The room code is for discovery
and is not an authorization secret.

## UDP endpoint pool

Bear has no directly reachable public UDP address, so the room service consumes
a static pool of tunnel mappings. For example:

| Bear UDP | Public tunnel |
| --- | --- |
| `5100` | `147.185.221.10:30100` |
| `5101` | `147.185.221.10:30101` |
| `5102` | `147.185.221.10:30102` |

Configure those UDP tunnels in Playit first. Then pass every exact mapping:

```sh
./scripts/run-bear-matchmaker.sh \
  --game-endpoint 5100=147.185.221.10:30100 \
  --game-endpoint 5101=147.185.221.10:30101 \
  --game-endpoint 5102=147.185.221.10:30102 \
  --public-http-url https://arena.example.com
```

The script deploys and builds `arena-matchmaker`, `arena-server`, and
`arena-admin`, preserves the existing file-backed server keys, starts the HTTP
service in the Bear `ai:openstrike-matchmaker` tmux window, and verifies
`/healthz`.

Cloudflare Tunnel carries only the HTTPS room API. Ordinary Cloudflare
published applications do not expose arbitrary public UDP. Playit carries the
Renet datagrams. Cloudflare Spectrum could replace Playit, but custom UDP
Spectrum is an Enterprise paid add-on.

## Lifecycle and capacity

- A waiting room expires after five minutes by default.
- A full room occupies exactly one endpoint and one `arena-server` process.
- Connect tokens expire after ten minutes by default.
- A normal match exits five seconds after `MatchEnded`; the matchmaker reaps
  the child and releases its endpoint.
- A disconnect after match terms exist cancels unreleased holds for reachable
  payees, awards the remaining player a forfeit result, and follows the same
  exit path.
- A child that never receives a client exits when its connect-token window
  closes; one that becomes empty before terms exist exits after 30 seconds.
- The matchmaker kills any remaining child when the room's match TTL expires
  or when it shuts down.

The number of `--game-endpoint` arguments is the hard concurrency limit. Start
with two to four endpoints on Bear. This is intentionally process-per-room:
failure and game state are isolated, and the implementation can later move to
containers or another scheduler without changing the client API.

## Security and operational limits

- Put the HTTP API behind HTTPS, Cloudflare rate limiting, and a request-size
  limit. The service itself limits request bodies to 4 KiB and caps waiting
  rooms.
- Treat a room ticket and returned Netcode token as bearer credentials. Do not
  log them, paste them into public chats, or cache their responses.
- The Netcode and settlement signing keys remain `0600` files on Bear.
- The service validates names and compressed secp256k1 pubkeys before issuing
  any token. A token binds name, seat, and FNN pubkey and cannot be edited by
  the client.
- This does not solve independent game-result arbitration. The authoritative
  game server remains the trusted oracle within the match's pre-authorized
  payment cap.
- A disconnected player's local FNN may be unreachable for immediate invoice
  cancellation. Its bounded hold-payment timeout and invoice expiry remain the
  final recovery mechanism.
- Reconnect, cross-process persisted match recovery, spectator mode, ranking,
  and automatic Fiber channel opening are outside this small public-demo
  service.
