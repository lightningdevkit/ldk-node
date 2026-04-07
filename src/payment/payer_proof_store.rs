// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::Mutex;

use lightning::ln::channelmanager::PaymentId;
use lightning::offers::payer_proof::PaidBolt12Invoice;

/// In-memory store for payer proof contexts, keyed by payment ID.
///
/// Stores the [`PaidBolt12Invoice`] received in [`Event::PaymentSent`] so that
/// payer proofs can be constructed later via [`Bolt12Payment::create_payer_proof`].
///
/// Note: This store is not persisted to disk. Payer proof contexts are lost on
/// restart. This is a known limitation of the current upstream LDK API where
/// [`PaidBolt12Invoice`] cannot be serialized and reconstructed externally.
///
/// [`Event::PaymentSent`]: lightning::events::Event::PaymentSent
/// [`Bolt12Payment::create_payer_proof`]: crate::payment::bolt12::Bolt12Payment::create_payer_proof
pub(crate) struct PayerProofContextStore {
	entries: Mutex<HashMap<PaymentId, PaidBolt12Invoice>>,
}

impl PayerProofContextStore {
	pub fn new() -> Self {
		Self { entries: Mutex::new(HashMap::new()) }
	}

	pub fn insert_or_update(&self, payment_id: PaymentId, paid_invoice: PaidBolt12Invoice) {
		self.entries.lock().unwrap().insert(payment_id, paid_invoice);
	}

	pub fn get(&self, payment_id: &PaymentId) -> Option<PaidBolt12Invoice> {
		self.entries.lock().unwrap().get(payment_id).cloned()
	}

	pub fn remove(&self, payment_id: &PaymentId) {
		self.entries.lock().unwrap().remove(payment_id);
	}
}
