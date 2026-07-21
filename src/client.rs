use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::protocol::{HoldInvoiceRelease, MatchTerms, PlayerSlot};

/// Verifies that every released preimage belongs to a hold invoice accepted in
/// the signed match context. The payer has already committed the match cap
/// before play starts; releases therefore never invoke the payer's wallet.
pub struct SettlementGuard {
    terms: MatchTerms,
    processed: HashSet<u16>,
    released_total: [u128; 2],
}

impl SettlementGuard {
    pub fn new(terms: MatchTerms) -> Self {
        Self {
            terms,
            processed: HashSet::new(),
            released_total: [0; 2],
        }
    }

    pub fn terms(&self) -> &MatchTerms {
        &self.terms
    }

    pub fn released_total(&self, payer: PlayerSlot) -> u128 {
        self.released_total[payer.index()]
    }

    pub fn validate_release(
        &mut self,
        release: &HoldInvoiceRelease,
        local_slot: PlayerSlot,
        now_ms: u64,
    ) -> Result<(), String> {
        let intent = &release.intent;
        if intent.body.match_id != self.terms.match_id {
            return Err("wrong match id".into());
        }
        intent
            .verify(&self.terms.server_verifying_key, now_ms)
            .map_err(|error| error.to_string())?;
        if intent.body.payee != local_slot {
            return Err("settlement release belongs to another payee".into());
        }
        let term = self
            .terms
            .hold_invoices
            .iter()
            .find(|term| term.reservation_id == intent.body.reservation_id)
            .ok_or_else(|| "unknown hold-invoice reservation".to_string())?;
        if term.payer != intent.body.payer
            || term.payee != intent.body.payee
            || term.amount != intent.body.amount
            || term.payment_hash != intent.body.payment_hash
        {
            return Err("settlement release does not match reserved invoice".into());
        }
        let computed_hash: [u8; 32] = Sha256::digest(release.payment_preimage).into();
        if computed_hash != term.payment_hash {
            return Err("settlement preimage does not match payment hash".into());
        }
        if !self.processed.insert(term.reservation_id) {
            return Err("duplicate hold-invoice release".into());
        }
        let payer_index = term.payer.index();
        let next_total = self.released_total[payer_index].saturating_add(term.amount);
        if next_total > self.terms.max_total_per_player {
            return Err("settlement release exceeds match cap".into());
        }
        self.released_total[payer_index] = next_total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::{
        net::{MOCK_FIBER_PUBKEY_A, MOCK_FIBER_PUBKEY_B},
        protocol::{
            HoldInvoiceTerm, PlayerBinding, SettlementIntent, SettlementReason,
            UnsignedSettlementIntent,
        },
    };

    fn setup() -> (SettlementGuard, SigningKey, [u8; 32]) {
        let key = SigningKey::from_bytes(&[8; 32]);
        let preimage = [5; 32];
        let payment_hash = Sha256::digest(preimage).into();
        let terms = MatchTerms {
            match_id: 7,
            amount_per_damage_bucket: 100,
            damage_bucket: 25,
            max_total_per_player: 200,
            payment_deadline_ms: 500,
            invoice_expiry_seconds: 7_200,
            hold_payment_timeout_seconds: 3_600,
            final_expiry_delta_ms: 9_600_000,
            server_verifying_key: key.verifying_key().to_bytes(),
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
            hold_invoices: vec![HoldInvoiceTerm {
                reservation_id: 0,
                payer: PlayerSlot::A,
                payee: PlayerSlot::B,
                amount: 100,
                payment_hash,
            }],
        };
        (SettlementGuard::new(terms), key, preimage)
    }

    fn release(key: &SigningKey, preimage: [u8; 32]) -> HoldInvoiceRelease {
        let payment_hash = Sha256::digest(preimage).into();
        HoldInvoiceRelease {
            intent: SettlementIntent::sign(
                UnsignedSettlementIntent {
                    match_id: 7,
                    sequence: 1,
                    reservation_id: 0,
                    game_tick: 1,
                    payer: PlayerSlot::A,
                    payee: PlayerSlot::B,
                    amount: 100,
                    payment_hash,
                    reason: SettlementReason::Damage { amount: 25 },
                    state_hash: [0; 32],
                    expires_at_ms: 1_000,
                },
                key,
            ),
            payment_preimage: preimage,
        }
    }

    #[test]
    fn verifies_signature_reservation_preimage_and_duplicate() {
        let (mut guard, key, preimage) = setup();
        let release = release(&key, preimage);
        guard
            .validate_release(&release, PlayerSlot::B, 100)
            .unwrap();
        assert_eq!(guard.released_total(PlayerSlot::A), 100);
        assert!(
            guard
                .validate_release(&release, PlayerSlot::B, 100)
                .is_err()
        );
    }

    #[test]
    fn rejects_wrong_preimage_before_marking_processed() {
        let (mut guard, key, preimage) = setup();
        let mut release = release(&key, preimage);
        release.payment_preimage = [9; 32];
        assert!(
            guard
                .validate_release(&release, PlayerSlot::B, 100)
                .is_err()
        );
    }
}
