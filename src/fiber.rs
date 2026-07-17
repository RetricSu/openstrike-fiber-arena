use std::{collections::BTreeMap, time::Duration};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{PaymentStatus, SettlementAck, SettlementIntent};

const MATCH_RECORD_KEY: &str = "0x46534201";

#[derive(Clone)]
pub struct FiberRpcClient {
    endpoint: String,
    http: Client,
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
    #[error("Fiber payment did not finish before the deadline")]
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

    pub async fn execute_intent(
        &self,
        intent: &SettlementIntent,
        target_pubkey: &str,
        timeout: Duration,
    ) -> Result<SettlementAck, FiberError> {
        let mut custom_records = BTreeMap::new();
        custom_records.insert(
            MATCH_RECORD_KEY,
            format!("0x{}", hex::encode(intent.record_hash())),
        );
        let params = json!([{
            "target_pubkey": target_pubkey,
            "amount": format!("0x{:x}", intent.body.amount),
            "keysend": true,
            "timeout": timeout.as_secs().max(1),
            "max_fee_amount": "0x0",
            "custom_records": custom_records,
        }]);
        let result = self.call("send_payment", params).await?;
        let hash_text = result
            .get("payment_hash")
            .and_then(Value::as_str)
            .ok_or(FiberError::MissingPaymentHash)?;
        let payment_hash = decode_hash(hash_text)?;
        let initial_status = parse_status(&result);
        if matches!(
            initial_status,
            PaymentStatus::Success | PaymentStatus::Failed { .. }
        ) {
            return Ok(ack(intent, payment_hash, initial_status));
        }

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(FiberError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let result = self
                .call("get_payment", json!([{ "payment_hash": hash_text }]))
                .await?;
            let status = parse_status(&result);
            if matches!(
                status,
                PaymentStatus::Success | PaymentStatus::Failed { .. }
            ) {
                return Ok(ack(intent, payment_hash, status));
            }
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

pub fn mock_success_ack(intent: &SettlementIntent) -> SettlementAck {
    let payment_hash = *blake3::hash(&intent.record_hash()).as_bytes();
    ack(intent, payment_hash, PaymentStatus::Success)
}

fn ack(intent: &SettlementIntent, payment_hash: [u8; 32], status: PaymentStatus) -> SettlementAck {
    SettlementAck {
        match_id: intent.body.match_id,
        settlement_sequence: intent.body.sequence,
        payment_hash: Some(payment_hash),
        status,
    }
}

fn parse_status(result: &Value) -> PaymentStatus {
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
        .to_ascii_lowercase();
    match status.as_str() {
        "success" => PaymentStatus::Success,
        "failed" => PaymentStatus::Failed {
            error: result
                .get("failed_error")
                .and_then(Value::as_str)
                .unwrap_or("unknown Fiber payment failure")
                .to_owned(),
        },
        _ => PaymentStatus::Pending,
    }
}

fn decode_hash(value: &str) -> Result<[u8; 32], FiberError> {
    let bytes =
        hex::decode(value.trim_start_matches("0x")).map_err(|_| FiberError::InvalidPaymentHash)?;
    bytes.try_into().map_err(|_| FiberError::InvalidPaymentHash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_payment_states() {
        assert_eq!(
            parse_status(&json!({"status": "Success"})),
            PaymentStatus::Success
        );
        assert!(matches!(
            parse_status(&json!({"status": "Failed", "failed_error": "no route"})),
            PaymentStatus::Failed { error } if error == "no route"
        ));
        assert_eq!(
            parse_status(&json!({"status": "Inflight"})),
            PaymentStatus::Pending
        );
    }

    #[test]
    fn decodes_prefixed_hash() {
        assert_eq!(
            decode_hash(&format!("0x{}", "ab".repeat(32))).unwrap(),
            [0xab; 32]
        );
    }
}
