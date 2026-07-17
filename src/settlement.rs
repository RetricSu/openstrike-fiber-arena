use std::collections::{BTreeMap, HashMap};

use ed25519_dalek::SigningKey;

use crate::protocol::{
    MatchTerms, PaymentStatus, PlayerSlot, SettlementAck, SettlementIntent, SettlementReason,
    UnsignedSettlementIntent,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerStatus {
    Pending,
    Success,
    Failed,
}

#[derive(Clone, Debug)]
pub struct LedgerEntry {
    pub intent: SettlementIntent,
    pub status: LedgerStatus,
    pub payment_hash: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettlementError {
    #[error("ack belongs to a different match")]
    WrongMatch,
    #[error("unknown settlement sequence {0}")]
    UnknownSequence(u64),
    #[error("settlement acknowledgement did not come from the expected payer")]
    WrongPayer,
    #[error("payer has reached the match payment cap")]
    PaymentCapReached,
}

pub struct SettlementCoordinator {
    terms: MatchTerms,
    signing_key: SigningKey,
    next_sequence: u64,
    damage_remainder: [u16; 2],
    issued_total: [u128; 2],
    entries: BTreeMap<u64, LedgerEntry>,
    max_pending_per_player: usize,
}

impl SettlementCoordinator {
    pub fn new(
        match_id: u128,
        signing_key: SigningKey,
        amount_per_damage_bucket: u128,
        damage_bucket: u16,
        max_total_per_player: u128,
        payment_deadline_ms: u64,
    ) -> Self {
        assert!(damage_bucket > 0, "damage bucket must be non-zero");
        let terms = MatchTerms {
            match_id,
            amount_per_damage_bucket,
            damage_bucket,
            max_total_per_player,
            payment_deadline_ms,
            server_verifying_key: signing_key.verifying_key().to_bytes(),
        };
        Self {
            terms,
            signing_key,
            next_sequence: 1,
            damage_remainder: [0; 2],
            issued_total: [0; 2],
            entries: BTreeMap::new(),
            max_pending_per_player: 2,
        }
    }

    pub fn terms(&self) -> &MatchTerms {
        &self.terms
    }

    pub fn latest_sequence(&self) -> u64 {
        self.next_sequence.saturating_sub(1)
    }

    pub fn issued_total(&self, payer: PlayerSlot) -> u128 {
        self.issued_total[payer.index()]
    }

    pub fn entry(&self, sequence: u64) -> Option<&LedgerEntry> {
        self.entries.get(&sequence)
    }

    pub fn record_damage(
        &mut self,
        attacker: PlayerSlot,
        victim: PlayerSlot,
        damage: u16,
        game_tick: u64,
        state_hash: [u8; 32],
        now_ms: u64,
    ) -> Result<Vec<SettlementIntent>, SettlementError> {
        debug_assert_eq!(attacker, victim.opponent());
        let payer_index = victim.index();
        self.damage_remainder[payer_index] =
            self.damage_remainder[payer_index].saturating_add(damage);
        let mut intents = Vec::new();

        while self.damage_remainder[payer_index] >= self.terms.damage_bucket {
            let next_total =
                self.issued_total[payer_index].saturating_add(self.terms.amount_per_damage_bucket);
            if next_total > self.terms.max_total_per_player {
                return Err(SettlementError::PaymentCapReached);
            }
            self.damage_remainder[payer_index] -= self.terms.damage_bucket;
            self.issued_total[payer_index] = next_total;

            let body = UnsignedSettlementIntent {
                match_id: self.terms.match_id,
                sequence: self.next_sequence,
                game_tick,
                payer: victim,
                payee: attacker,
                amount: self.terms.amount_per_damage_bucket,
                reason: SettlementReason::Damage {
                    amount: self.terms.damage_bucket,
                },
                state_hash,
                expires_at_ms: now_ms.saturating_add(self.terms.payment_deadline_ms),
            };
            self.next_sequence += 1;
            let intent = SettlementIntent::sign(body, &self.signing_key);
            self.entries.insert(
                intent.body.sequence,
                LedgerEntry {
                    intent: intent.clone(),
                    status: LedgerStatus::Pending,
                    payment_hash: None,
                },
            );
            intents.push(intent);
        }
        Ok(intents)
    }

    pub fn acknowledge(
        &mut self,
        payer: PlayerSlot,
        ack: SettlementAck,
    ) -> Result<(), SettlementError> {
        if ack.match_id != self.terms.match_id {
            return Err(SettlementError::WrongMatch);
        }
        let entry = self
            .entries
            .get_mut(&ack.settlement_sequence)
            .ok_or(SettlementError::UnknownSequence(ack.settlement_sequence))?;
        if entry.intent.body.payer != payer {
            return Err(SettlementError::WrongPayer);
        }

        match (&entry.status, ack.status) {
            (LedgerStatus::Success, _) => return Ok(()),
            (_, PaymentStatus::Pending) => entry.status = LedgerStatus::Pending,
            (_, PaymentStatus::Success) => entry.status = LedgerStatus::Success,
            (_, PaymentStatus::Failed { .. }) => entry.status = LedgerStatus::Failed,
        }
        entry.payment_hash = ack.payment_hash;
        Ok(())
    }

    pub fn blocking_payer(&self, now_ms: u64) -> Option<PlayerSlot> {
        let mut pending_count = HashMap::<PlayerSlot, usize>::new();
        for entry in self.entries.values() {
            match entry.status {
                LedgerStatus::Failed => return Some(entry.intent.body.payer),
                LedgerStatus::Pending if now_ms > entry.intent.body.expires_at_ms => {
                    return Some(entry.intent.body.payer);
                }
                LedgerStatus::Pending => {
                    let count = pending_count.entry(entry.intent.body.payer).or_default();
                    *count += 1;
                    if *count > self.max_pending_per_player {
                        return Some(entry.intent.body.payer);
                    }
                }
                LedgerStatus::Success => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator() -> SettlementCoordinator {
        SettlementCoordinator::new(42, SigningKey::from_bytes(&[3; 32]), 100, 25, 1_000, 500)
    }

    #[test]
    fn damage_is_bucketed_into_signed_intents() {
        let mut coordinator = coordinator();
        assert!(
            coordinator
                .record_damage(PlayerSlot::A, PlayerSlot::B, 24, 1, [1; 32], 100)
                .unwrap()
                .is_empty()
        );
        let intents = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 26, 2, [2; 32], 100)
            .unwrap();
        assert_eq!(intents.len(), 2);
        assert_eq!(intents[0].body.payer, PlayerSlot::B);
        assert_eq!(coordinator.issued_total(PlayerSlot::B), 200);
        intents[0]
            .verify(&coordinator.terms().server_verifying_key, 200)
            .unwrap();
    }

    #[test]
    fn successful_ack_is_idempotent() {
        let mut coordinator = coordinator();
        let intent = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 1, [1; 32], 0)
            .unwrap()
            .remove(0);
        let ack = SettlementAck {
            match_id: 42,
            settlement_sequence: intent.body.sequence,
            payment_hash: Some([9; 32]),
            status: PaymentStatus::Success,
        };
        coordinator.acknowledge(PlayerSlot::B, ack.clone()).unwrap();
        coordinator.acknowledge(PlayerSlot::B, ack).unwrap();
        assert_eq!(
            coordinator.entry(intent.body.sequence).unwrap().status,
            LedgerStatus::Success
        );
    }

    #[test]
    fn expired_payment_blocks_its_payer() {
        let mut coordinator = coordinator();
        coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 1, [1; 32], 100)
            .unwrap();
        assert_eq!(coordinator.blocking_payer(599), None);
        assert_eq!(coordinator.blocking_payer(601), Some(PlayerSlot::B));
    }

    #[test]
    fn opponent_cannot_acknowledge_someone_elses_payment() {
        let mut coordinator = coordinator();
        let intent = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 1, [1; 32], 0)
            .unwrap()
            .remove(0);
        let error = coordinator
            .acknowledge(
                PlayerSlot::A,
                SettlementAck {
                    match_id: 42,
                    settlement_sequence: intent.body.sequence,
                    payment_hash: Some([9; 32]),
                    status: PaymentStatus::Success,
                },
            )
            .unwrap_err();
        assert!(matches!(error, SettlementError::WrongPayer));
    }
}
