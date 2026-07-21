use ed25519_dalek::SigningKey;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::protocol::{
    HoldInvoiceAck, HoldInvoiceFailure, HoldInvoiceOffer, HoldInvoiceRelease, HoldInvoiceStage,
    HoldInvoiceTerm, MatchTerms, PlayerBinding, PlayerSlot, SettlementIntent, SettlementReason,
    UnsignedSettlementIntent,
};

const MAX_INVOICE_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationStatus {
    AwaitingInvoice,
    Offered,
    Held,
    Released,
    Settled,
    CancelPending,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug)]
struct HoldReservation {
    term: HoldInvoiceTerm,
    payment_preimage: [u8; 32],
    invoice: Option<String>,
    payer_funded: bool,
    payee_received: bool,
    status: ReservationStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum SettlementError {
    #[error("message belongs to a different match")]
    WrongMatch,
    #[error("unknown hold-invoice reservation {0}")]
    UnknownReservation(u16),
    #[error("payment hash does not match reservation {0}")]
    WrongPaymentHash(u16),
    #[error("hold invoice came from the wrong player")]
    WrongInvoiceOwner,
    #[error("hold-invoice acknowledgement came from the wrong player")]
    WrongAcknowledgementOwner,
    #[error("hold invoice is too large")]
    InvoiceTooLarge,
    #[error("reservation already contains a different invoice")]
    ConflictingInvoice,
    #[error("reservation {0} has no invoice yet")]
    MissingInvoice(u16),
    #[error("reservation {0} is not ready for this transition")]
    InvalidReservationState(u16),
    #[error("payer has no held invoice remaining")]
    PaymentCapReached,
}

pub struct SettlementCoordinator {
    terms: MatchTerms,
    signing_key: SigningKey,
    next_sequence: u64,
    damage_remainder: [u16; 2],
    released_total: [u128; 2],
    reservations: Vec<HoldReservation>,
}

#[allow(clippy::too_many_arguments)]
impl SettlementCoordinator {
    pub fn new(
        match_id: u128,
        signing_key: SigningKey,
        players: [PlayerBinding; 2],
        amount_per_damage_bucket: u128,
        damage_bucket: u16,
        max_total_per_player: u128,
        payment_deadline_ms: u64,
        invoice_expiry_seconds: u64,
        hold_payment_timeout_seconds: u64,
        final_expiry_delta_ms: u64,
    ) -> Self {
        assert!(damage_bucket > 0, "damage bucket must be non-zero");
        assert!(
            amount_per_damage_bucket > 0,
            "payment per bucket must be non-zero"
        );
        // Use the arithmetic form so this stays compatible with Rust versions
        // that predate the integer `is_multiple_of` method.
        #[allow(clippy::manual_is_multiple_of)]
        let cap_is_aligned = max_total_per_player % amount_per_damage_bucket == 0;
        assert!(
            max_total_per_player >= amount_per_damage_bucket && cap_is_aligned,
            "match cap must be a positive multiple of the per-bucket amount"
        );
        let reservations_per_player = max_total_per_player / amount_per_damage_bucket;
        assert!(
            reservations_per_player.saturating_mul(2) <= u16::MAX as u128,
            "too many hold invoices for one match"
        );

        let mut reservations = Vec::with_capacity((reservations_per_player * 2) as usize);
        let mut terms_reservations = Vec::with_capacity((reservations_per_player * 2) as usize);
        for payer in PlayerSlot::ALL {
            for _ in 0..reservations_per_player {
                let mut payment_preimage = [0u8; 32];
                OsRng.fill_bytes(&mut payment_preimage);
                let payment_hash: [u8; 32] = Sha256::digest(payment_preimage).into();
                let term = HoldInvoiceTerm {
                    reservation_id: terms_reservations.len() as u16,
                    payer,
                    payee: payer.opponent(),
                    amount: amount_per_damage_bucket,
                    payment_hash,
                };
                terms_reservations.push(term.clone());
                reservations.push(HoldReservation {
                    term,
                    payment_preimage,
                    invoice: None,
                    payer_funded: false,
                    payee_received: false,
                    status: ReservationStatus::AwaitingInvoice,
                });
            }
        }

        let terms = MatchTerms {
            match_id,
            amount_per_damage_bucket,
            damage_bucket,
            max_total_per_player,
            payment_deadline_ms,
            invoice_expiry_seconds,
            hold_payment_timeout_seconds,
            final_expiry_delta_ms,
            server_verifying_key: signing_key.verifying_key().to_bytes(),
            players,
            hold_invoices: terms_reservations,
        };
        Self {
            terms,
            signing_key,
            next_sequence: 1,
            damage_remainder: [0; 2],
            released_total: [0; 2],
            reservations,
        }
    }

    pub fn terms(&self) -> &MatchTerms {
        &self.terms
    }

    pub fn latest_sequence(&self) -> u64 {
        self.next_sequence.saturating_sub(1)
    }

    pub fn released_total(&self, payer: PlayerSlot) -> u128 {
        self.released_total[payer.index()]
    }

    pub fn reservation_status(&self, reservation_id: u16) -> Option<ReservationStatus> {
        self.reservations
            .get(reservation_id as usize)
            .map(|reservation| reservation.status)
    }

    pub fn register_invoice_offer(
        &mut self,
        sender: PlayerSlot,
        offer: HoldInvoiceOffer,
    ) -> Result<PlayerSlot, SettlementError> {
        self.validate_reference(offer.match_id, offer.reservation_id, offer.payment_hash)?;
        if offer.invoice.len() > MAX_INVOICE_BYTES {
            return Err(SettlementError::InvoiceTooLarge);
        }
        let reservation = self.reservation_mut(offer.reservation_id)?;
        if reservation.term.payee != sender {
            return Err(SettlementError::WrongInvoiceOwner);
        }
        if let Some(current) = &reservation.invoice {
            if current != &offer.invoice {
                return Err(SettlementError::ConflictingInvoice);
            }
            return Ok(reservation.term.payer);
        }
        if reservation.status != ReservationStatus::AwaitingInvoice {
            return Err(SettlementError::InvalidReservationState(
                offer.reservation_id,
            ));
        }
        reservation.invoice = Some(offer.invoice);
        reservation.status = ReservationStatus::Offered;
        Ok(reservation.term.payer)
    }

    pub fn acknowledge(
        &mut self,
        sender: PlayerSlot,
        ack: HoldInvoiceAck,
    ) -> Result<(), SettlementError> {
        self.validate_reference(ack.match_id, ack.reservation_id, ack.payment_hash)?;
        let reservation = self.reservation_mut(ack.reservation_id)?;
        let expected_sender = match ack.stage {
            HoldInvoiceStage::Funded => reservation.term.payer,
            HoldInvoiceStage::Received
            | HoldInvoiceStage::Settled
            | HoldInvoiceStage::Cancelled => reservation.term.payee,
        };
        if sender != expected_sender {
            return Err(SettlementError::WrongAcknowledgementOwner);
        }

        match ack.stage {
            HoldInvoiceStage::Funded => {
                if reservation.invoice.is_none() {
                    return Err(SettlementError::MissingInvoice(ack.reservation_id));
                }
                if matches!(
                    reservation.status,
                    ReservationStatus::AwaitingInvoice
                        | ReservationStatus::Released
                        | ReservationStatus::Settled
                        | ReservationStatus::CancelPending
                        | ReservationStatus::Cancelled
                        | ReservationStatus::Failed
                ) {
                    return Err(SettlementError::InvalidReservationState(ack.reservation_id));
                }
                reservation.payer_funded = true;
            }
            HoldInvoiceStage::Received => {
                if reservation.invoice.is_none() {
                    return Err(SettlementError::MissingInvoice(ack.reservation_id));
                }
                if matches!(
                    reservation.status,
                    ReservationStatus::Released
                        | ReservationStatus::Settled
                        | ReservationStatus::CancelPending
                        | ReservationStatus::Cancelled
                        | ReservationStatus::Failed
                ) {
                    return Err(SettlementError::InvalidReservationState(ack.reservation_id));
                }
                reservation.payee_received = true;
            }
            HoldInvoiceStage::Settled => {
                if reservation.status == ReservationStatus::Settled {
                    return Ok(());
                }
                if reservation.status != ReservationStatus::Released {
                    return Err(SettlementError::InvalidReservationState(ack.reservation_id));
                }
                reservation.status = ReservationStatus::Settled;
                return Ok(());
            }
            HoldInvoiceStage::Cancelled => {
                if reservation.status == ReservationStatus::Cancelled {
                    return Ok(());
                }
                if reservation.status != ReservationStatus::CancelPending {
                    return Err(SettlementError::InvalidReservationState(ack.reservation_id));
                }
                reservation.status = ReservationStatus::Cancelled;
                return Ok(());
            }
        }

        if reservation.payer_funded && reservation.payee_received {
            reservation.status = ReservationStatus::Held;
        }
        Ok(())
    }

    pub fn record_failure(
        &mut self,
        sender: PlayerSlot,
        failure: &HoldInvoiceFailure,
    ) -> Result<(), SettlementError> {
        self.validate_reference(
            failure.match_id,
            failure.reservation_id,
            failure.payment_hash,
        )?;
        let reservation = self.reservation_mut(failure.reservation_id)?;
        let expected_sender = match failure.stage {
            HoldInvoiceStage::Funded => reservation.term.payer,
            HoldInvoiceStage::Received
            | HoldInvoiceStage::Settled
            | HoldInvoiceStage::Cancelled => reservation.term.payee,
        };
        if sender != expected_sender {
            return Err(SettlementError::WrongAcknowledgementOwner);
        }
        if matches!(
            failure.stage,
            HoldInvoiceStage::Funded | HoldInvoiceStage::Received
        ) {
            reservation.status = ReservationStatus::Failed;
        }
        Ok(())
    }

    pub fn all_held(&self) -> bool {
        !self.reservations.is_empty()
            && self
                .reservations
                .iter()
                .all(|reservation| reservation.status == ReservationStatus::Held)
    }

    pub fn record_damage(
        &mut self,
        attacker: PlayerSlot,
        victim: PlayerSlot,
        damage: u16,
        game_tick: u64,
        state_hash: [u8; 32],
        now_ms: u64,
    ) -> Result<Vec<HoldInvoiceRelease>, SettlementError> {
        debug_assert_eq!(attacker, victim.opponent());
        let payer_index = victim.index();
        self.damage_remainder[payer_index] =
            self.damage_remainder[payer_index].saturating_add(damage);
        let mut releases = Vec::new();

        while self.damage_remainder[payer_index] >= self.terms.damage_bucket {
            let reservation_index = self
                .reservations
                .iter()
                .position(|reservation| {
                    reservation.term.payer == victim
                        && reservation.status == ReservationStatus::Held
                })
                .ok_or(SettlementError::PaymentCapReached)?;
            self.damage_remainder[payer_index] -= self.terms.damage_bucket;

            let reservation = &mut self.reservations[reservation_index];
            let body = UnsignedSettlementIntent {
                match_id: self.terms.match_id,
                sequence: self.next_sequence,
                reservation_id: reservation.term.reservation_id,
                game_tick,
                payer: victim,
                payee: attacker,
                amount: reservation.term.amount,
                payment_hash: reservation.term.payment_hash,
                reason: SettlementReason::Damage {
                    amount: self.terms.damage_bucket,
                },
                state_hash,
                expires_at_ms: now_ms.saturating_add(self.terms.payment_deadline_ms),
            };
            self.next_sequence += 1;
            self.released_total[payer_index] =
                self.released_total[payer_index].saturating_add(reservation.term.amount);
            reservation.status = ReservationStatus::Released;
            releases.push(HoldInvoiceRelease {
                intent: SettlementIntent::sign(body, &self.signing_key),
                payment_preimage: reservation.payment_preimage,
            });
        }
        Ok(releases)
    }

    pub fn cancel_unused(&mut self) -> Vec<HoldInvoiceTerm> {
        let mut terms = Vec::new();
        for reservation in &mut self.reservations {
            if matches!(
                reservation.status,
                ReservationStatus::Offered | ReservationStatus::Held
            ) {
                reservation.status = ReservationStatus::CancelPending;
                terms.push(reservation.term.clone());
            }
        }
        terms
    }

    fn validate_reference(
        &self,
        match_id: u128,
        reservation_id: u16,
        payment_hash: [u8; 32],
    ) -> Result<(), SettlementError> {
        if match_id != self.terms.match_id {
            return Err(SettlementError::WrongMatch);
        }
        let reservation = self.reservation(reservation_id)?;
        if reservation.term.payment_hash != payment_hash {
            return Err(SettlementError::WrongPaymentHash(reservation_id));
        }
        Ok(())
    }

    fn reservation(&self, reservation_id: u16) -> Result<&HoldReservation, SettlementError> {
        self.reservations
            .get(reservation_id as usize)
            .filter(|reservation| reservation.term.reservation_id == reservation_id)
            .ok_or(SettlementError::UnknownReservation(reservation_id))
    }

    fn reservation_mut(
        &mut self,
        reservation_id: u16,
    ) -> Result<&mut HoldReservation, SettlementError> {
        self.reservations
            .get_mut(reservation_id as usize)
            .filter(|reservation| reservation.term.reservation_id == reservation_id)
            .ok_or(SettlementError::UnknownReservation(reservation_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{MOCK_FIBER_PUBKEY_A, MOCK_FIBER_PUBKEY_B};

    fn players() -> [PlayerBinding; 2] {
        [
            PlayerBinding {
                name: "alice".into(),
                fiber_pubkey: MOCK_FIBER_PUBKEY_A.into(),
            },
            PlayerBinding {
                name: "bob".into(),
                fiber_pubkey: MOCK_FIBER_PUBKEY_B.into(),
            },
        ]
    }

    fn coordinator() -> SettlementCoordinator {
        SettlementCoordinator::new(
            42,
            SigningKey::from_bytes(&[3; 32]),
            players(),
            100,
            25,
            200,
            500,
            7_200,
            3_600,
            9_600_000,
        )
    }

    fn prepare_all(coordinator: &mut SettlementCoordinator) {
        for term in coordinator.terms().hold_invoices.clone() {
            let offer = HoldInvoiceOffer {
                match_id: 42,
                reservation_id: term.reservation_id,
                payment_hash: term.payment_hash,
                invoice: format!("invoice-{}", term.reservation_id),
            };
            coordinator
                .register_invoice_offer(term.payee, offer)
                .unwrap();
            for (sender, stage) in [
                (term.payer, HoldInvoiceStage::Funded),
                (term.payee, HoldInvoiceStage::Received),
            ] {
                coordinator
                    .acknowledge(
                        sender,
                        HoldInvoiceAck {
                            match_id: 42,
                            reservation_id: term.reservation_id,
                            payment_hash: term.payment_hash,
                            stage,
                        },
                    )
                    .unwrap();
            }
        }
    }

    #[test]
    fn both_parties_must_confirm_every_hold_invoice_before_start() {
        let mut coordinator = coordinator();
        assert!(!coordinator.all_held());
        prepare_all(&mut coordinator);
        assert!(coordinator.all_held());
    }

    #[test]
    fn opponent_cannot_offer_an_invoice_for_the_payee() {
        let mut coordinator = coordinator();
        let term = coordinator.terms().hold_invoices[0].clone();
        let error = coordinator
            .register_invoice_offer(
                term.payer,
                HoldInvoiceOffer {
                    match_id: 42,
                    reservation_id: term.reservation_id,
                    payment_hash: term.payment_hash,
                    invoice: "wrong-owner".into(),
                },
            )
            .unwrap_err();
        assert!(matches!(error, SettlementError::WrongInvoiceOwner));
    }

    #[test]
    fn damage_releases_server_preimage_from_a_held_invoice() {
        let mut coordinator = coordinator();
        prepare_all(&mut coordinator);
        let release = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 8, [7; 32], 100)
            .unwrap()
            .remove(0);
        release
            .intent
            .verify(&coordinator.terms().server_verifying_key, 200)
            .unwrap();
        assert_eq!(release.intent.body.payer, PlayerSlot::B);
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(release.payment_preimage)),
            release.intent.body.payment_hash
        );
        assert_eq!(coordinator.released_total(PlayerSlot::B), 100);
    }

    #[test]
    fn damage_cannot_issue_unfunded_credit() {
        let mut coordinator = coordinator();
        let error = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 1, [1; 32], 100)
            .unwrap_err();
        assert!(matches!(error, SettlementError::PaymentCapReached));
    }

    #[test]
    fn match_end_only_cancels_unreleased_invoices() {
        let mut coordinator = coordinator();
        prepare_all(&mut coordinator);
        let release = coordinator
            .record_damage(PlayerSlot::A, PlayerSlot::B, 25, 1, [1; 32], 100)
            .unwrap()
            .remove(0);
        let unused = coordinator.cancel_unused();
        assert_eq!(unused.len(), 3);
        assert!(
            !unused
                .iter()
                .any(|term| { term.reservation_id == release.intent.body.reservation_id })
        );
    }
}
