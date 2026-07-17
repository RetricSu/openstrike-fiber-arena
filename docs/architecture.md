# Architecture

## Runtime flow

```text
Player A client ---- InputFrame ----\
                                    +--> authoritative arena server
Player B client ---- InputFrame ----/             |
                                                 | SettlementIntent
                            WorldSnapshot <-------+-------> client FiberAdapter
                                                               |
                                                               v
                                                     local Fiber node RPC
                                                               |
                                                     direct Fiber channel
```

`SettlementIntent` contains a monotonically increasing sequence, match and game
tick identifiers, payer/payee slots, amount, state hash, expiry, and an Ed25519
server signature. The game process never receives a wallet private key.

## Trust model

The server is the match oracle: it decides hits and signs settlement intents.
Each player explicitly accepts match terms containing the per-event and total
payment caps. A local adapter refuses unsigned, expired, duplicate, or
over-budget intents.

Opening a funded channel does not let the opponent pull money. A player can stop
their adapter and refuse a payment. The current implementation limits this
exposure with a small credit window: gameplay pauses while an intent is overdue.
A timed forfeit rule can be layered on once match lifecycle policy is fixed.

This is not trustless game-result arbitration. Adding that later requires
signed input logs and a replay/challenge mechanism.

## Transport channels

| Renet channel | Direction | Payload |
| --- | --- | --- |
| unreliable | client -> server | sequenced input frames |
| unreliable | server -> client | authoritative snapshots |
| reliable ordered | both | join, match lifecycle, settlement intent/ack |

Production clients receive a short-lived Netcode `ConnectToken` from the
matchmaker. The token authenticates the client id and player metadata and
contains per-connection encryption keys; only the server/token issuer knows the
32-byte Netcode private key. `--dev-unsecure` is an explicit local-only escape
hatch.

Settlement intents use a separate Ed25519 key so transport authentication and
payment authorization do not share a secret or failure domain. Clients receive
the verification key in accepted match terms. The server loads the signing seed
from a mode-`0600` file and has no implicit production fallback.

## Fiber channel shape

For the first direct P2P demo, each player pre-funds their maximum exposure:

```text
A -- one-way channel --> B
B -- one-way channel --> A
```

Payments are triggered by authoritative damage buckets rather than frames. The
default harness emits one payment per 25 damage. Fiber payment failures never
block the 64 Hz simulation thread; they are handled by an asynchronous worker.
