use std::collections::HashSet;

use crate::protocol::{MatchTerms, SettlementIntent};

/// Client-side spending policy accepted before a match. Both the headless
/// driver and the native desktop client use this exact verifier before a
/// payment can reach the player's local Fiber node.
pub struct SettlementGuard {
    terms: MatchTerms,
    processed: HashSet<u64>,
    committed_total: u128,
}

impl SettlementGuard {
    pub fn new(terms: MatchTerms) -> Self {
        Self {
            terms,
            processed: HashSet::new(),
            committed_total: 0,
        }
    }

    pub fn terms(&self) -> &MatchTerms {
        &self.terms
    }

    pub fn committed_total(&self) -> u128 {
        self.committed_total
    }

    pub fn validate(&mut self, intent: &SettlementIntent, now_ms: u64) -> Result<(), String> {
        if intent.body.match_id != self.terms.match_id {
            return Err("wrong match id".into());
        }
        intent
            .verify(&self.terms.server_verifying_key, now_ms)
            .map_err(|error| error.to_string())?;
        if intent.body.amount > self.terms.amount_per_damage_bucket {
            return Err("intent exceeds per-event cap".into());
        }
        if self.processed.contains(&intent.body.sequence) {
            return Err("duplicate settlement sequence".into());
        }
        let next_total = self.committed_total.saturating_add(intent.body.amount);
        if next_total > self.terms.max_total_per_player {
            return Err("intent exceeds match spending cap".into());
        }
        self.processed.insert(intent.body.sequence);
        self.committed_total = next_total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::protocol::{
        PlayerSlot, SettlementIntent, SettlementReason, UnsignedSettlementIntent,
    };

    fn setup() -> (SettlementGuard, SigningKey) {
        let key = SigningKey::from_bytes(&[8; 32]);
        let terms = MatchTerms {
            match_id: 7,
            amount_per_damage_bucket: 100,
            damage_bucket: 25,
            max_total_per_player: 200,
            payment_deadline_ms: 500,
            server_verifying_key: key.verifying_key().to_bytes(),
        };
        (SettlementGuard::new(terms), key)
    }

    fn intent(key: &SigningKey, sequence: u64, amount: u128) -> SettlementIntent {
        SettlementIntent::sign(
            UnsignedSettlementIntent {
                match_id: 7,
                sequence,
                game_tick: sequence,
                payer: PlayerSlot::A,
                payee: PlayerSlot::B,
                amount,
                reason: SettlementReason::Damage { amount: 25 },
                state_hash: [0; 32],
                expires_at_ms: 1_000,
            },
            key,
        )
    }

    #[test]
    fn enforces_signature_duplicates_and_total_cap() {
        let (mut guard, key) = setup();
        guard.validate(&intent(&key, 1, 100), 100).unwrap();
        assert!(guard.validate(&intent(&key, 1, 100), 100).is_err());
        guard.validate(&intent(&key, 2, 100), 100).unwrap();
        assert!(guard.validate(&intent(&key, 3, 100), 100).is_err());
        assert_eq!(guard.committed_total(), 200);
    }
}
