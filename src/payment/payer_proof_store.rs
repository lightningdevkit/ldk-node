// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::blinded_path::message::OffersContext;
use lightning::impl_writeable_tlv_based;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::nonce::Nonce;

use crate::data_store::{StorableObject, StorableObjectUpdate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PayerProofContext {
	pub payment_id: PaymentId,
	pub invoice: Bolt12Invoice,
	pub nonce: Nonce,
}

impl PayerProofContext {
	pub(crate) fn from_invoice_received(
		payment_id: PaymentId, invoice: &Bolt12Invoice, context: Option<&OffersContext>,
	) -> Option<Self> {
		match context {
			Some(OffersContext::OutboundPaymentForOffer {
				payment_id: context_payment_id,
				nonce,
			})
			| Some(OffersContext::OutboundPaymentForRefund {
				payment_id: context_payment_id,
				nonce,
			}) if *context_payment_id == payment_id => {
				Some(Self { payment_id, invoice: invoice.clone(), nonce: *nonce })
			},
			_ => None,
		}
	}
}

impl_writeable_tlv_based!(PayerProofContext, {
	(0, payment_id, required),
	(2, invoice, required),
	(4, nonce, required),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PayerProofContextUpdate {
	pub payment_id: PaymentId,
	pub invoice: Option<Bolt12Invoice>,
	pub nonce: Option<Nonce>,
}

impl From<&PayerProofContext> for PayerProofContextUpdate {
	fn from(value: &PayerProofContext) -> Self {
		Self {
			payment_id: value.payment_id,
			invoice: Some(value.invoice.clone()),
			nonce: Some(value.nonce),
		}
	}
}

impl StorableObject for PayerProofContext {
	type Id = PaymentId;
	type Update = PayerProofContextUpdate;

	fn id(&self) -> Self::Id {
		self.payment_id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(
			self.payment_id, update.payment_id,
			"We should only ever override payer proof context for the same payment id"
		);

		let mut updated = false;

		if let Some(invoice) = update.invoice {
			if self.invoice != invoice {
				self.invoice = invoice;
				updated = true;
			}
		}

		if let Some(nonce) = update.nonce {
			if self.nonce != nonce {
				self.nonce = nonce;
				updated = true;
			}
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

impl StorableObjectUpdate<PayerProofContext> for PayerProofContextUpdate {
	fn id(&self) -> <PayerProofContext as StorableObject>::Id {
		self.payment_id
	}
}
