# Fiber integration status

The adapter targets the official FNN `v0.9.0-rc7` release at commit
`bc361aaaa40d1394b83e6a1808869b0b06c48c13`. It deliberately has no PeerId
compatibility path: `node_info.pubkey`, `list_channels.pubkey`, invoice
`payee_public_key`, and `send_payment` all use the compressed secp256k1 node
identity pubkey.

## Implemented flow

```text
server creates random preimages and publishes SHA-256 hashes
        |
payee FNN new_invoice(payment_hash, hash_algorithm=sha256)
        |
payer FNN parse_invoice + field checks + send_payment(invoice)
        |
payee FNN get_invoice == Received
        |
both directions ready -> match starts
        |
authoritative damage bucket -> signed server preimage release
        |
payee FNN settle_invoice(payment_hash, payment_preimage)
        |
direct Fiber liquidity changes off-chain
```

The default is 25 damage and 1,000 shannons per bucket, with a 4,000-shannon
cap per player. That creates four reservations in each direction. The 64 Hz
simulation never waits for an FNN RPC during live play; settlement and cleanup
run on asynchronous workers.

Before accepting real-payment terms, each client checks:

- FNN reports version `0.9.0-rc7`;
- local `node_info.pubkey` matches its encrypted connect-token binding;
- the opponent pubkey comes from the other authenticated slot;
- an enabled `ChannelReady` direct channel exists and can send the full cap;
- every incoming invoice parsed by the payer's FNN is signed and has the exact
  currency, amount, payment hash, SHA-256 algorithm, match/reservation
  description, invoice expiry, final expiry delta, and bound payee pubkey.

The implementation uses hexadecimal JSON quantities required by rc7. Hold
invoices default to a two-hour invoice expiry, a one-hour payer-side payment
timeout, and the FNN production minimum `final_expiry_delta` of 9,600,000 ms.

## Verified rc7 funded behavior

On 2026-07-21 the complete flow was exercised on the CKB testnet with
`fiber-pay` CLI `0.3.0` and two independent official arm64 macOS FNN
`v0.9.0-rc7` nodes:

- both `node_info` identities matched their slot-bound connect tokens;
- a private bidirectional channel reached `ChannelReady` with channel ID
  `0x2da2db9dcd9d40cb385ce7b8a1b431c1cdd09173d45ceb7552bc3383943fc0a9`;
- the CKB funding transaction
  `0xb38daa558202fd6d3f3a550ba44df0d284ebd42c9b34999e9598b609539c0fcc`
  was committed on testnet;
- eight 1,000-shannon SHA-256 hold invoices reached `Received` before the
  authoritative match started;
- seven damage buckets reached `Settled`, while the unused eighth reservation
  reached `Cancelled` after the match ended;
- there were no pending TLCs after cleanup;
- the direct-channel liquid balances changed from
  `100,000,000 / 10,000,000,000` shannons to
  `100,001,000 / 9,999,999,000` shannons. The 1,000-shannon net transfer and
  unchanged total independently match the seven directional settlements.

For CKB channels, FNN's `funding_amount` is the total CKB locked by that side,
not the resulting liquid balance. Approximately 99 CKB per side is reserved
for settlement capacity. For example, funding 100 CKB produces about 1 CKB of
liquid outbound balance. Operators must size the total funding amount with this
reserve in mind.

## Local readiness check

```sh
cargo run --bin fiber-probe -- \
  --fiber-rpc http://127.0.0.1:8227 --node-only

cargo run --bin fiber-probe -- \
  --fiber-rpc http://127.0.0.1:8227 \
  --peer-pubkey 03... --required-outbound 4000
```

`fiber-probe` uses `--peer-pubkey` only as an operator diagnostic. The game
client does not accept an opponent override; its command is:

```text
--fiber-rpc http://127.0.0.1:8227 --fiber-currency Fibt
```

The RPC must remain on localhost or a trusted private interface.

## Funded test prerequisites

1. Run one rc7 FNN instance per player with distinct RPC/P2P ports and data
   directories.
2. Back up each encrypted secret and `FIBER_SECRET_KEY_PASSWORD`.
3. Fund both wallets on the selected network.
4. Connect the two identity pubkeys and open one channel with outbound balance
   in each direction, or a bidirectional channel.
5. Wait for `ChannelReady`.
6. Issue slot A/B connect tokens containing the exact two `node_info.pubkey`
   values.
7. Start each client with only its own localhost RPC URL and currency.

No script creates, funds, or closes a channel automatically.

## Current limitations

### Server trust remains

The server owns the preimages and is the game oracle. It can release an invoice
without legitimate damage. Exposure is bounded by the pre-authorized match cap;
this design does not attempt trustless result arbitration.

### No portable server-side payment receipt

The server requires both a payer `Funded` report and a payee `Received` report.
This prevents unilateral player cheating, but neither report is an FNN-signed
receipt independently verifiable by the server. FNN rc7 does not expose such a
portable receipt through the public RPC.

### Exact channel path is not pinned

The client requires a direct channel and sends the invoice with zero fee and a
single part, so the direct edge is the expected route. FNN's normal
`send_payment` router could still choose another zero-fee path. rc7 can pin a
channel through `build_router` + `send_payment_with_router`, but that API does
not expose the payer-side timeout used here. The MVP favors bounded lock time;
an exact-outpoint route should be added when both constraints are available.

### Cleanup and recovery

Unused invoices are cancelled by the payee client. If it disconnects or refuses,
the payer's FNN payment timeout is the fallback. There is no reconnect recovery,
forfeit policy, or persisted server ledger/preimage store yet. A server restart
during a match loses its in-memory coordinator, although player-side timeouts
still bound unused locks.

### Release candidate stability

The build intentionally rejects other FNN versions. rc interfaces and storage
formats may still change; migration and backup procedures must be reviewed
before moving funded nodes to another release.
