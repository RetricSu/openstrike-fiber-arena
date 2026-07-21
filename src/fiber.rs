use std::{collections::BTreeMap, fmt, str::FromStr, time::Duration};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    net::normalize_fiber_pubkey,
    protocol::{HoldInvoiceTerm, MatchTerms},
};

// FNN v0.9.0-rc7 limits custom record keys to 16 bits. This remains useful for
// the standalone keysend route probe, although game settlement uses invoices.
const MATCH_RECORD_KEY: &str = "0x4653";
const SUPPORTED_FNN_VERSION: &str = "0.9.0-rc7";

#[derive(Clone)]
pub struct FiberRpcClient {
    endpoint: String,
    http: Client,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiberCurrency {
    Mainnet,
    Testnet,
    Devnet,
}

impl FiberCurrency {
    pub const fn rpc_name(self) -> &'static str {
        match self {
            Self::Mainnet => "Fibb",
            Self::Testnet => "Fibt",
            Self::Devnet => "Fibd",
        }
    }
}

impl fmt::Display for FiberCurrency {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.rpc_name())
    }
}

impl FromStr for FiberCurrency {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "fibb" | "mainnet" => Ok(Self::Mainnet),
            "fibt" | "testnet" => Ok(Self::Testnet),
            "fibd" | "devnet" => Ok(Self::Devnet),
            _ => Err("currency must be Fibb/mainnet, Fibt/testnet, or Fibd/devnet".into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiberNodeInfo {
    pub version: String,
    pub pubkey: String,
    pub chain_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiberDirectChannel {
    pub channel_id: String,
    pub local_balance: u128,
    pub remote_balance: u128,
    pub is_one_way: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiberReadiness {
    pub node: FiberNodeInfo,
    pub channel: FiberDirectChannel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldInvoiceExpectation {
    pub match_id: u128,
    pub reservation_id: u16,
    pub amount: u128,
    pub payment_hash: [u8; 32],
    pub payee_pubkey: String,
    pub currency: FiberCurrency,
    pub invoice_expiry_seconds: u64,
    pub final_expiry_delta_ms: u64,
}

impl HoldInvoiceExpectation {
    pub fn new(
        terms: &MatchTerms,
        term: &HoldInvoiceTerm,
        payee_pubkey: &str,
        currency: FiberCurrency,
    ) -> Result<Self, FiberError> {
        if term.payer == term.payee
            || !terms
                .hold_invoices
                .iter()
                .any(|candidate| candidate == term)
        {
            return Err(FiberError::InvoiceMismatch(
                "reservation is not part of the accepted match terms".into(),
            ));
        }
        Ok(Self {
            match_id: terms.match_id,
            reservation_id: term.reservation_id,
            amount: term.amount,
            payment_hash: term.payment_hash,
            payee_pubkey: normalize_fiber_pubkey(payee_pubkey)
                .map_err(FiberError::InvalidPubkey)?,
            currency,
            invoice_expiry_seconds: terms.invoice_expiry_seconds,
            final_expiry_delta_ms: terms.final_expiry_delta_ms,
        })
    }

    fn description(&self) -> String {
        format!("openstrike:{}:{}", self.match_id, self.reservation_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiberInvoiceStatus {
    Open,
    Received,
    Paid,
    Cancelled,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedInvoice {
    currency: String,
    amount: u128,
    payment_hash: [u8; 32],
    payee_pubkey: String,
    hash_algorithm: String,
    description: String,
    expiry_seconds: u64,
    final_expiry_delta_ms: u64,
    signed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum FiberError {
    #[error("Fiber RPC request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Fiber RPC returned an error: {0}")]
    Rpc(String),
    #[error("Fiber RPC response did not include a payment hash")]
    MissingPaymentHash,
    #[error("Fiber payment hash was malformed")]
    InvalidPaymentHash,
    #[error("invalid Fiber pubkey: {0}")]
    InvalidPubkey(String),
    #[error("Fiber RPC response was malformed: {0}")]
    InvalidResponse(String),
    #[error("Fiber RPC returned an invalid hexadecimal quantity for {field}: {value}")]
    InvalidQuantity { field: &'static str, value: String },
    #[error("FNN {found} is not supported; this build targets {SUPPORTED_FNN_VERSION}")]
    UnsupportedVersion { found: String },
    #[error("the configured Fiber peer is this local node")]
    TargetIsLocalNode,
    #[error("no ready outbound direct Fiber channel to {0}")]
    NoReadyDirectChannel(String),
    #[error(
        "direct Fiber channel outbound balance is {available} shannons, but the match requires {required}"
    )]
    InsufficientDirectBalance { available: u128, required: u128 },
    #[error("hold invoice does not match reservation: {0}")]
    InvoiceMismatch(String),
    #[error("Fiber payment failed: {0}")]
    PaymentFailed(String),
    #[error("invoice reached unexpected terminal status {0:?}")]
    UnexpectedInvoiceStatus(FiberInvoiceStatus),
    #[error("Fiber operation did not finish before the deadline")]
    Timeout,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

impl FiberRpcClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            http: Client::new(),
        }
    }

    pub async fn node_info(&self) -> Result<FiberNodeInfo, FiberError> {
        let result = self.call("node_info", json!([])).await?;
        let version = required_string(&result, "version")?.to_owned();
        if !version
            .trim_start_matches('v')
            .starts_with(SUPPORTED_FNN_VERSION)
        {
            return Err(FiberError::UnsupportedVersion { found: version });
        }
        Ok(FiberNodeInfo {
            version,
            pubkey: normalize_fiber_pubkey(required_string(&result, "pubkey")?)
                .map_err(FiberError::InvalidPubkey)?,
            chain_hash: required_string(&result, "chain_hash")?.to_owned(),
        })
    }

    /// FNN v0.9.0-rc7 identifies channel counterparties by their identity pubkey.
    pub async fn check_direct_channel(
        &self,
        target_pubkey: &str,
        required_outbound: u128,
    ) -> Result<FiberReadiness, FiberError> {
        let target_pubkey =
            normalize_fiber_pubkey(target_pubkey).map_err(FiberError::InvalidPubkey)?;
        let node = self.node_info().await?;
        if node.pubkey == target_pubkey {
            return Err(FiberError::TargetIsLocalNode);
        }

        let result = self
            .call(
                "list_channels",
                json!([{
                    "pubkey": target_pubkey,
                    "include_closed": false,
                }]),
            )
            .await?;
        let channel = select_direct_channel(&result, &target_pubkey, required_outbound)?;
        Ok(FiberReadiness { node, channel })
    }

    pub async fn create_hold_invoice(
        &self,
        expectation: &HoldInvoiceExpectation,
    ) -> Result<String, FiberError> {
        let result = self
            .call("new_invoice", new_hold_invoice_params(expectation))
            .await?;
        let address = required_string(&result, "invoice_address")?.to_owned();
        let parsed = parse_invoice_object(result.get("invoice").ok_or_else(|| {
            FiberError::InvalidResponse("new_invoice result omitted `invoice`".into())
        })?)?;
        validate_invoice(&parsed, expectation)?;
        Ok(address)
    }

    /// Parses the payee-signed invoice with the payer's own FNN, validates all
    /// security-critical fields, dry-runs the exact invoice route, then commits
    /// the hold payment. A Created/Inflight response is expected: it cannot
    /// become Success until the server later releases the preimage.
    pub async fn fund_hold_invoice(
        &self,
        invoice: &str,
        expectation: &HoldInvoiceExpectation,
        timeout_seconds: u64,
    ) -> Result<[u8; 32], FiberError> {
        let parsed_result = self
            .call("parse_invoice", json!([{ "invoice": invoice }]))
            .await?;
        let parsed = parse_invoice_object(parsed_result.get("invoice").ok_or_else(|| {
            FiberError::InvalidResponse("parse_invoice result omitted `invoice`".into())
        })?)?;
        validate_invoice(&parsed, expectation)?;

        let dry_run = self
            .call(
                "send_payment",
                invoice_payment_params(invoice, timeout_seconds, true),
            )
            .await?;
        ensure_payment_not_failed(&dry_run)?;
        let result = self
            .call(
                "send_payment",
                invoice_payment_params(invoice, timeout_seconds, false),
            )
            .await?;
        ensure_payment_not_failed(&result)?;
        let payment_hash = decode_hash(
            result
                .get("payment_hash")
                .and_then(Value::as_str)
                .ok_or(FiberError::MissingPaymentHash)?,
        )?;
        if payment_hash != expectation.payment_hash {
            return Err(FiberError::InvoiceMismatch(
                "send_payment returned a different payment hash".into(),
            ));
        }
        Ok(payment_hash)
    }

    pub async fn wait_invoice_received(
        &self,
        payment_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<(), FiberError> {
        self.wait_invoice_status(payment_hash, FiberInvoiceStatus::Received, timeout)
            .await
    }

    pub async fn settle_hold_invoice(
        &self,
        payment_hash: [u8; 32],
        payment_preimage: [u8; 32],
        timeout: Duration,
    ) -> Result<(), FiberError> {
        self.call(
            "settle_invoice",
            json!([{
                "payment_hash": hash_hex(payment_hash),
                "payment_preimage": hash_hex(payment_preimage),
            }]),
        )
        .await?;
        self.wait_invoice_status(payment_hash, FiberInvoiceStatus::Paid, timeout)
            .await
    }

    pub async fn cancel_hold_invoice(&self, payment_hash: [u8; 32]) -> Result<(), FiberError> {
        let result = self
            .call(
                "cancel_invoice",
                json!([{ "payment_hash": hash_hex(payment_hash) }]),
            )
            .await?;
        let status = invoice_status(&result)?;
        if status != FiberInvoiceStatus::Cancelled {
            return Err(FiberError::UnexpectedInvoiceStatus(status));
        }
        Ok(())
    }

    /// Retained for the standalone route probe. Game settlement does not use
    /// keysend after the hold-invoice protocol is enabled.
    pub async fn dry_run_keysend(
        &self,
        target_pubkey: &str,
        amount: u128,
        timeout: Duration,
    ) -> Result<(), FiberError> {
        self.call(
            "send_payment",
            keysend_params(target_pubkey, amount, [0; 32], timeout, true),
        )
        .await?;
        Ok(())
    }

    async fn wait_invoice_status(
        &self,
        payment_hash: [u8; 32],
        wanted: FiberInvoiceStatus,
        timeout: Duration,
    ) -> Result<(), FiberError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let result = self
                .call(
                    "get_invoice",
                    json!([{ "payment_hash": hash_hex(payment_hash) }]),
                )
                .await?;
            let status = invoice_status(&result)?;
            if status == wanted
                || (wanted == FiberInvoiceStatus::Received && status == FiberInvoiceStatus::Paid)
            {
                return Ok(());
            }
            if matches!(
                status,
                FiberInvoiceStatus::Cancelled | FiberInvoiceStatus::Expired
            ) {
                return Err(FiberError::UnexpectedInvoiceStatus(status));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(FiberError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, FiberError> {
        let response: RpcResponse = self
            .http
            .post(&self.endpoint)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(error) = response.error {
            return Err(FiberError::Rpc(format!(
                "{} ({})",
                error.message, error.code
            )));
        }
        response
            .result
            .ok_or_else(|| FiberError::Rpc("missing result".into()))
    }
}

fn new_hold_invoice_params(expectation: &HoldInvoiceExpectation) -> Value {
    json!([{
        "amount": format!("0x{:x}", expectation.amount),
        "currency": expectation.currency.rpc_name(),
        "description": expectation.description(),
        "payment_hash": hash_hex(expectation.payment_hash),
        "expiry": format!("0x{:x}", expectation.invoice_expiry_seconds),
        "final_expiry_delta": format!("0x{:x}", expectation.final_expiry_delta_ms),
        "hash_algorithm": "sha256",
        "allow_mpp": false,
        "allow_trampoline_routing": false,
    }])
}

fn invoice_payment_params(invoice: &str, timeout_seconds: u64, dry_run: bool) -> Value {
    json!([{
        "invoice": invoice,
        "timeout": format!("0x{:x}", timeout_seconds.max(1)),
        "max_fee_amount": "0x0",
        "max_parts": "0x1",
        "dry_run": dry_run,
    }])
}

fn keysend_params(
    target_pubkey: &str,
    amount: u128,
    record_hash: [u8; 32],
    timeout: Duration,
    dry_run: bool,
) -> Value {
    let mut custom_records = BTreeMap::new();
    custom_records.insert(MATCH_RECORD_KEY, hash_hex(record_hash));
    json!([{
        "target_pubkey": target_pubkey,
        "amount": format!("0x{amount:x}"),
        "keysend": true,
        "timeout": format!("0x{:x}", timeout_seconds(timeout)),
        "max_fee_amount": "0x0",
        "custom_records": custom_records,
        "dry_run": dry_run,
    }])
}

fn timeout_seconds(timeout: Duration) -> u64 {
    timeout
        .as_secs()
        .saturating_add(u64::from(timeout.subsec_nanos() != 0))
        .max(1)
}

fn parse_invoice_object(value: &Value) -> Result<ParsedInvoice, FiberError> {
    let data = value
        .get("data")
        .ok_or_else(|| FiberError::InvalidResponse("invoice omitted `data`".into()))?;
    let attrs = data
        .get("attrs")
        .and_then(Value::as_array)
        .ok_or_else(|| FiberError::InvalidResponse("invoice omitted `data.attrs`".into()))?;
    let mut payee_pubkey = None;
    let mut hash_algorithm = None;
    let mut description = None;
    let mut expiry_seconds = None;
    let mut final_expiry_delta_ms = None;
    for attr in attrs {
        if let Some(value) = attr.get("payee_public_key").and_then(Value::as_str) {
            payee_pubkey = Some(normalize_fiber_pubkey(value).map_err(FiberError::InvalidPubkey)?);
        }
        if let Some(value) = attr.get("hash_algorithm").and_then(Value::as_str) {
            hash_algorithm = Some(value.to_owned());
        }
        if let Some(value) = attr.get("description").and_then(Value::as_str) {
            description = Some(value.to_owned());
        }
        if attr.get("expiry_time").is_some() {
            expiry_seconds = Some(hex_u64_quantity(attr, "expiry_time")?);
        }
        if attr.get("final_htlc_minimum_expiry_delta").is_some() {
            final_expiry_delta_ms =
                Some(hex_u64_quantity(attr, "final_htlc_minimum_expiry_delta")?);
        }
    }
    Ok(ParsedInvoice {
        currency: required_string(value, "currency")?.to_owned(),
        amount: hex_quantity(value, "amount")?,
        payment_hash: decode_hash(required_string(data, "payment_hash")?)?,
        payee_pubkey: payee_pubkey.ok_or_else(|| {
            FiberError::InvalidResponse("invoice omitted payee_public_key".into())
        })?,
        hash_algorithm: hash_algorithm
            .ok_or_else(|| FiberError::InvalidResponse("invoice omitted hash_algorithm".into()))?,
        description: description
            .ok_or_else(|| FiberError::InvalidResponse("invoice omitted description".into()))?,
        expiry_seconds: expiry_seconds
            .ok_or_else(|| FiberError::InvalidResponse("invoice omitted expiry_time".into()))?,
        final_expiry_delta_ms: final_expiry_delta_ms.ok_or_else(|| {
            FiberError::InvalidResponse("invoice omitted final_htlc_minimum_expiry_delta".into())
        })?,
        signed: value
            .get("signature")
            .and_then(Value::as_str)
            .is_some_and(|signature| !signature.is_empty()),
    })
}

fn validate_invoice(
    invoice: &ParsedInvoice,
    expected: &HoldInvoiceExpectation,
) -> Result<(), FiberError> {
    let mismatch = if !invoice.signed {
        Some("invoice is unsigned".to_string())
    } else if invoice.currency != expected.currency.rpc_name() {
        Some(format!(
            "currency is {}, expected {}",
            invoice.currency, expected.currency
        ))
    } else if invoice.amount != expected.amount {
        Some(format!(
            "amount is {}, expected {}",
            invoice.amount, expected.amount
        ))
    } else if invoice.payment_hash != expected.payment_hash {
        Some("payment hash differs from the server reservation".into())
    } else if invoice.payee_pubkey != expected.payee_pubkey {
        Some("payee pubkey differs from the authenticated player binding".into())
    } else if !invoice.hash_algorithm.eq_ignore_ascii_case("sha256") {
        Some("hash algorithm is not sha256".into())
    } else if invoice.description != expected.description() {
        Some("description differs from the accepted match reservation".into())
    } else if invoice.expiry_seconds != expected.invoice_expiry_seconds {
        Some(format!(
            "invoice expiry is {}, expected {} seconds",
            invoice.expiry_seconds, expected.invoice_expiry_seconds
        ))
    } else if invoice.final_expiry_delta_ms != expected.final_expiry_delta_ms {
        Some(format!(
            "final expiry delta is {}, expected {} ms",
            invoice.final_expiry_delta_ms, expected.final_expiry_delta_ms
        ))
    } else {
        None
    };
    mismatch.map_or(Ok(()), |message| Err(FiberError::InvoiceMismatch(message)))
}

fn ensure_payment_not_failed(result: &Value) -> Result<(), FiberError> {
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("Created");
    if status.eq_ignore_ascii_case("Failed") {
        return Err(FiberError::PaymentFailed(
            result
                .get("failed_error")
                .and_then(Value::as_str)
                .unwrap_or("unknown Fiber payment failure")
                .to_owned(),
        ));
    }
    Ok(())
}

fn invoice_status(result: &Value) -> Result<FiberInvoiceStatus, FiberError> {
    match required_string(result, "status")? {
        "Open" => Ok(FiberInvoiceStatus::Open),
        "Received" => Ok(FiberInvoiceStatus::Received),
        "Paid" => Ok(FiberInvoiceStatus::Paid),
        "Cancelled" => Ok(FiberInvoiceStatus::Cancelled),
        "Expired" => Ok(FiberInvoiceStatus::Expired),
        value => Err(FiberError::InvalidResponse(format!(
            "unknown invoice status `{value}`"
        ))),
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, FiberError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| FiberError::InvalidResponse(format!("missing string field `{field}`")))
}

fn select_direct_channel(
    result: &Value,
    target_pubkey: &str,
    required_outbound: u128,
) -> Result<FiberDirectChannel, FiberError> {
    let channels = result
        .get("channels")
        .and_then(Value::as_array)
        .ok_or_else(|| FiberError::InvalidResponse("missing array field `channels`".into()))?;
    let mut best: Option<FiberDirectChannel> = None;

    for value in channels {
        let ready = value
            .get("state")
            .and_then(|state| state.get("state_name"))
            .and_then(Value::as_str)
            == Some("ChannelReady");
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_one_way = value
            .get("is_one_way")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_acceptor = value
            .get("is_acceptor")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !ready || !enabled || (is_one_way && is_acceptor) {
            continue;
        }

        let channel = FiberDirectChannel {
            channel_id: required_string(value, "channel_id")?.to_owned(),
            local_balance: hex_quantity(value, "local_balance")?,
            remote_balance: hex_quantity(value, "remote_balance")?,
            is_one_way,
        };
        if best
            .as_ref()
            .is_none_or(|current| channel.local_balance > current.local_balance)
        {
            best = Some(channel);
        }
    }

    let channel = best.ok_or_else(|| FiberError::NoReadyDirectChannel(target_pubkey.to_owned()))?;
    if channel.local_balance < required_outbound {
        return Err(FiberError::InsufficientDirectBalance {
            available: channel.local_balance,
            required: required_outbound,
        });
    }
    Ok(channel)
}

fn hex_quantity(value: &Value, field: &'static str) -> Result<u128, FiberError> {
    let text = required_string(value, field)?;
    u128::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).map_err(|_| {
        FiberError::InvalidQuantity {
            field,
            value: text.to_owned(),
        }
    })
}

fn hex_u64_quantity(value: &Value, field: &'static str) -> Result<u64, FiberError> {
    let text = required_string(value, field)?;
    u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).map_err(|_| {
        FiberError::InvalidQuantity {
            field,
            value: text.to_owned(),
        }
    })
}

fn decode_hash(value: &str) -> Result<[u8; 32], FiberError> {
    let bytes =
        hex::decode(value.trim_start_matches("0x")).map_err(|_| FiberError::InvalidPaymentHash)?;
    bytes.try_into().map_err(|_| FiberError::InvalidPaymentHash)
}

fn hash_hex(value: [u8; 32]) -> String {
    format!("0x{}", hex::encode(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        net::{MOCK_FIBER_PUBKEY_A, MOCK_FIBER_PUBKEY_B},
        protocol::{PlayerBinding, PlayerSlot},
    };

    fn expectation() -> HoldInvoiceExpectation {
        let term = HoldInvoiceTerm {
            reservation_id: 3,
            payer: PlayerSlot::A,
            payee: PlayerSlot::B,
            amount: 1_000,
            payment_hash: [0xab; 32],
        };
        let terms = MatchTerms {
            match_id: 9,
            amount_per_damage_bucket: 1_000,
            damage_bucket: 25,
            max_total_per_player: 4_000,
            payment_deadline_ms: 30_000,
            invoice_expiry_seconds: 7_200,
            hold_payment_timeout_seconds: 3_600,
            final_expiry_delta_ms: 9_600_000,
            server_verifying_key: [0; 32],
            players: [
                PlayerBinding {
                    name: "alice".into(),
                    fiber_pubkey: MOCK_FIBER_PUBKEY_A.into(),
                },
                PlayerBinding {
                    name: "bob".into(),
                    fiber_pubkey: MOCK_FIBER_PUBKEY_B.into(),
                },
            ],
            hold_invoices: vec![term.clone()],
        };
        HoldInvoiceExpectation::new(&terms, &term, MOCK_FIBER_PUBKEY_B, FiberCurrency::Testnet)
            .unwrap()
    }

    #[test]
    fn hold_invoice_uses_rc7_hex_quantities_and_sha256() {
        let params = new_hold_invoice_params(&expectation());
        let request = &params[0];
        assert_eq!(request["amount"], "0x3e8");
        assert_eq!(request["currency"], "Fibt");
        assert_eq!(request["payment_hash"], format!("0x{}", "ab".repeat(32)));
        assert_eq!(request["hash_algorithm"], "sha256");
        assert_eq!(request["final_expiry_delta"], "0x927c00");
    }

    #[test]
    fn parses_and_validates_rc7_invoice_result() {
        let value = json!({
            "currency": "Fibt",
            "amount": "0x3e8",
            "signature": "01deadbeef",
            "data": {
                "payment_hash": format!("0x{}", "ab".repeat(32)),
                "attrs": [
                    {"description": "openstrike:9:3"},
                    {"expiry_time": "0x1c20"},
                    {"final_htlc_minimum_expiry_delta": "0x927c00"},
                    {"hash_algorithm": "sha256"},
                    {"payee_public_key": MOCK_FIBER_PUBKEY_B}
                ]
            }
        });
        let parsed = parse_invoice_object(&value).unwrap();
        validate_invoice(&parsed, &expectation()).unwrap();
    }

    #[test]
    fn rejects_invoice_outside_bound_identity_or_lock_terms() {
        let mut parsed = ParsedInvoice {
            currency: "Fibt".into(),
            amount: 1_000,
            payment_hash: [0xab; 32],
            payee_pubkey: crate::net::MOCK_FIBER_PUBKEY_A.into(),
            hash_algorithm: "sha256".into(),
            description: "openstrike:9:3".into(),
            expiry_seconds: 7_200,
            final_expiry_delta_ms: 9_600_000,
            signed: true,
        };
        assert!(matches!(
            validate_invoice(&parsed, &expectation()),
            Err(FiberError::InvoiceMismatch(_))
        ));
        parsed.payee_pubkey = MOCK_FIBER_PUBKEY_B.into();
        validate_invoice(&parsed, &expectation()).unwrap();

        parsed.expiry_seconds += 1;
        assert!(matches!(
            validate_invoice(&parsed, &expectation()),
            Err(FiberError::InvoiceMismatch(_))
        ));
        parsed.expiry_seconds -= 1;
        parsed.final_expiry_delta_ms += 1;
        assert!(matches!(
            validate_invoice(&parsed, &expectation()),
            Err(FiberError::InvoiceMismatch(_))
        ));
    }

    #[test]
    fn keysend_probe_uses_supported_record_key() {
        let params = keysend_params(
            MOCK_FIBER_PUBKEY_B,
            1_000,
            [0xab; 32],
            Duration::from_millis(1_001),
            true,
        );
        let request = &params[0];
        assert_eq!(request["timeout"], "0x2");
        assert_eq!(
            request["custom_records"][MATCH_RECORD_KEY],
            format!("0x{}", "ab".repeat(32))
        );
    }

    #[test]
    fn selects_ready_outbound_channel_with_enough_liquidity() {
        let result = json!({
            "channels": [{
                "channel_id": format!("0x{}", "11".repeat(32)),
                "pubkey": MOCK_FIBER_PUBKEY_B,
                "state": { "state_name": "ChannelReady" },
                "enabled": true,
                "is_one_way": true,
                "is_acceptor": false,
                "local_balance": "0x186a0",
                "remote_balance": "0x0"
            }]
        });
        let channel = select_direct_channel(&result, MOCK_FIBER_PUBKEY_B, 100_000).unwrap();
        assert_eq!(channel.local_balance, 100_000);
        assert!(channel.is_one_way);
    }

    #[test]
    fn one_way_acceptor_cannot_use_channel_for_outbound_payment() {
        let result = json!({
            "channels": [{
                "channel_id": format!("0x{}", "22".repeat(32)),
                "pubkey": MOCK_FIBER_PUBKEY_B,
                "state": { "state_name": "ChannelReady" },
                "enabled": true,
                "is_one_way": true,
                "is_acceptor": true,
                "local_balance": "0x186a0",
                "remote_balance": "0x0"
            }]
        });
        assert!(matches!(
            select_direct_channel(&result, MOCK_FIBER_PUBKEY_B, 1),
            Err(FiberError::NoReadyDirectChannel(_))
        ));
    }
}
