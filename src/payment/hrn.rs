// src/payment/hrn.rs

use crate::error::Error;
use crate::logger::{log_error, log_info, Logger};
use crate::payment::store::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus, PaymentStore};
use crate::types::ChannelManager;

use lightning::ln::channelmanager::{PaymentId, Retry};
use lightning::offers::offer::{Amount, Offer, Quantity};
use lightning::offers::parse::Bolt12SemanticError;
use lightning::util::string::UntrustedString;

use std::sync::{Arc, RwLock};

/// A payment handler for sending payments to Human-Readable Names (HRNs).
pub struct HrnPayment {
    runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
    channel_manager: Arc<ChannelManager>,
    payment_store: Arc<PaymentStore<Arc<Logger>>>,
    logger: Arc<Logger>,
}

impl HrnPayment {
    pub(crate) fn new(
        runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
        channel_manager: Arc<ChannelManager>,
        payment_store: Arc<PaymentStore<Arc<Logger>>>,
        logger: Arc<Logger>,
    ) -> Self {
        Self { runtime, channel_manager, payment_store, logger }
    }

    /// Send a payment to a Human-Readable Name (HRN).
    ///
    /// This method resolves the HRN to an offer and sends the payment.
    ///
    /// If `payer_note` is `Some`, it will be seen by the recipient and reflected back in the invoice.
    /// If `quantity` is `Some`, it represents the number of items requested.
    pub fn send_to_hrn(
        &self, hrn: &str, quantity: Option<u64>, payer_note: Option<String>,
    ) -> Result<PaymentId, Error> {
        let rt_lock = self.runtime.read().unwrap();
        if rt_lock.is_none() {
            return Err(Error::NotRunning);
        }

        // Resolve the HRN to an offer
        let offer = self.resolve_hrn_to_offer(hrn)?;

        // Use the existing payment logic to send the payment
        let mut random_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut random_bytes);
        let payment_id = PaymentId(random_bytes);
        let retry_strategy = Retry::Timeout(LDK_PAYMENT_RETRY_TIMEOUT);
        let max_total_routing_fee_msat = None;

        match self.channel_manager.pay_for_offer(
            &offer,
            quantity,
            None,
            payer_note.clone(),
            payment_id,
            retry_strategy,
            max_total_routing_fee_msat,
        ) {
            Ok(()) => {
                let payee_pubkey = offer.issuer_signing_pubkey();
                log_info!(
                    self.logger,
                    "Initiated sending payment to HRN: {} (payee: {:?})",
                    hrn,
                    payee_pubkey
                );

                let kind = PaymentKind::Bolt12Offer {
                    hash: None,
                    preimage: None,
                    secret: None,
                    offer_id: offer.id(),
                    payer_note: payer_note.map(UntrustedString),
                    quantity,
                };
                let payment = PaymentDetails::new(
                    payment_id,
                    kind,
                    None, // Amount will be set by the offer
                    PaymentDirection::Outbound,
                    PaymentStatus::Pending,
                );
                self.payment_store.insert(payment)?;

                Ok(payment_id)
            }
            Err(e) => {
                log_error!(self.logger, "Failed to send payment to HRN: {:?}", e);
                match e {
                    Bolt12SemanticError::DuplicatePaymentId => Err(Error::DuplicatePayment),
                    _ => Err(Error::PaymentSendingFailed),
                }
            }
        }
    }

    /// Resolves a Human-Readable Name (HRN) to an offer.
    ///
    /// This is a placeholder for actual HRN resolution logic.
    fn resolve_hrn_to_offer(&self, hrn: &str) -> Result<Offer, Error> {
        // Placeholder logic for resolving HRN to an offer
        log_info!(self.logger, "Resolving HRN: {}", hrn);

        // For now, return a mock offer
        let offer_builder = self.channel_manager.create_offer_builder(None).map_err(|e| {
            log_error!(self.logger, "Failed to create offer builder: {:?}", e);
            Error::OfferCreationFailed
        })?;

        let offer = offer_builder
            .amount_msats(1000) // Example amount
            .description(hrn.to_string())
            .build()
            .map_err(|e| {
                log_error!(self.logger, "Failed to create offer: {:?}", e);
                Error::OfferCreationFailed
            })?;

        Ok(offer)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::TestLogger;
    use crate::types::TestChannelManager;

    #[test]
    fn test_send_to_hrn() {
        let runtime = Arc::new(RwLock::new(Some(Arc::new(tokio::runtime::Runtime::new().unwrap()))));
        let channel_manager = Arc::new(TestChannelManager::new());
        let payment_store = Arc::new(PaymentStore::new(Vec::new(), Arc::new(TestStore::new(false)), Arc::new(TestLogger::new())));
        let logger = Arc::new(TestLogger::new());

        let hrn_payment = HrnPayment::new(runtime, channel_manager, payment_store, logger);
        let result = hrn_payment.send_to_hrn("example.hrn", None, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_hrn_to_offer() {
        let runtime = Arc::new(RwLock::new(Some(Arc::new(tokio::runtime::Runtime::new().unwrap()))));
        let channel_manager = Arc::new(TestChannelManager::new());
        let payment_store = Arc::new(PaymentStore::new(Vec::new(), Arc::new(TestStore::new(false)), Arc::new(TestLogger::new())));
        let logger = Arc::new(TestLogger::new());

        let hrn_payment = HrnPayment::new(runtime, channel_manager, payment_store, logger);
        let result = hrn_payment.resolve_hrn_to_offer("example.hrn");
        assert!(result.is_ok());
    }
}