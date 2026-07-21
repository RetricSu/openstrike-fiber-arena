# Architecture

## Runtime flow

```text
short-lived Netcode token
  = player name + fixed A/B seat + FNN identity pubkey
                         |
                         v
Player A client ---- InputFrame ----\
                                    +--> authoritative arena server
Player B client ---- InputFrame ----/             |
                            WorldSnapshot <--------+
                                                   |
                                  signed preimage release on damage
                                                   |
                                                   v
                                         payee's local FNN
                                           settle_invoice
                                                   |
                                         off-chain channel update
```

Renet and Fiber remain separate. Renet carries input, authoritative snapshots,
match setup, invoice offers, dual readiness acknowledgements, and preimages.
Each local FNN owns its wallet keys and performs all invoice/payment RPC calls.
The arena server never receives a wallet key or FNN RPC credential.

## Hold-invoice lifecycle

The server generates four random preimages per payer by default and publishes
only their SHA-256 hashes in `MatchTerms`.

1. The future payee creates a signed FNN hold invoice for each server hash.
2. The server forwards the invoice to the bound payer.
3. The payer asks its own FNN to parse the invoice and checks currency, amount,
   payment hash, SHA-256 algorithm, signature presence, match/reservation
   description, invoice expiry, final expiry delta, and the token-bound payee
   pubkey before calling `send_payment`.
4. The payer reports `Funded`; the payee independently polls `get_invoice` and
   reports `Received`. Both reports are required for every invoice.
5. The server starts the match only after both players accept the terms and all
   reservations are held.
6. Each authoritative damage bucket consumes one held reservation. The server
   signs the game event and sends the matching preimage only to the payee. The
   payee calls `settle_invoice`; the payer no longer has a refusal point.
7. At match end the server tells each payee to cancel unused invoices.

## Trust model

The server remains the trusted game oracle. It decides hits, damage, invoice
hashes, and when to release preimages. A malicious server can incorrectly
consume the amount pre-authorized for one match, but it never receives wallet
keys and cannot exceed the invoice set accepted before play.

The payment setup is resistant to one malicious player:

- a payer cannot start by fabricating `Funded`, because the payee must observe
  its own invoice in `Received`;
- a payee cannot redirect or inflate an invoice, because the payer's own FNN
  parses it and the client compares all security-critical fields;
- a payee can lie about `Received`, but that only starts without protecting its
  own future income.

This is not trustless game-result arbitration and it is not an independent
third-party proof of Fiber payment state. Those require a protocol-level signed
receipt or a server escrow/hub, neither of which is exposed by the current FNN
RPC without changing the P2P payment model.

## Identity and transport

Production clients use short-lived encrypted Netcode `ConnectToken`s. Token
user data contains the authorized player name, fixed `PlayerSlot`, and the
FNN v0.9.0-rc7 compressed secp256k1 identity pubkey. The server rejects duplicate
seats and waits for both bindings before constructing immutable match terms.
The client verifies that its local `node_info.pubkey` equals its token binding;
the opponent pubkey is taken from match terms rather than a command-line flag.

`--dev-unsecure` deliberately falls back to first-free seats and deterministic
mock pubkeys. It is for local/mock play only and cannot pass a real-FNN identity
check.

Settlement releases use a separate file-backed Ed25519 key so transport
authentication and game-event authorization do not share a secret.

| Renet channel | Direction | Payload |
| --- | --- | --- |
| unreliable | client -> server | sequenced input frames |
| unreliable | server -> client | authoritative snapshots |
| reliable ordered | both | setup, invoices, receipts, preimages, lifecycle |

## Fiber channel shape

For the symmetric 1v1 demo, each player needs reusable outbound liquidity:

```text
A FNN -- funded one-way channel --> B FNN
B FNN -- funded one-way channel --> A FNN
```

A bidirectional channel with sufficient balance on both sides also works. The
server never opens or closes channels. Match payments and hold settlements are
off-chain updates; channel lifecycle remains a CKB L1 operation.
