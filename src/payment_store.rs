use crate::hex_utils;
use crate::Error;

use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::util::persist::KVStorePersister;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use std::collections::HashMap;
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::Mutex;

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentInfo {
	/// The payment hash, i.e., the hash of the `preimage`.
	pub payment_hash: PaymentHash,
	/// The pre-image used by the payment.
	pub preimage: Option<PaymentPreimage>,
	/// The secret used by the payment.
	pub secret: Option<PaymentSecret>,
	/// The amount transferred.
	pub amount_msat: Option<u64>,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
}

impl_writeable_tlv_based!(PaymentInfo, {
	(0, payment_hash, required),
	(1, preimage, required),
	(2, secret, required),
	(3, amount_msat, required),
	(4, direction, required),
	(5, status, required)
});

/// Represents the direction of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentDirection {
	/// The payment is inbound.
	Inbound,
	/// The payment is outbound.
	Outbound,
}

impl_writeable_tlv_based_enum!(PaymentDirection,
	(0, Inbound) => {},
	(1, Outbound) => {};
);

/// Represents the current status of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending,
	/// The payment suceeded.
	Succeeded,
	/// The payment failed.
	Failed,
}

impl_writeable_tlv_based_enum!(PaymentStatus,
	(0, Pending) => {},
	(1, Succeeded) => {},
	(2, Failed) => {};
);

/// The payment information will be persisted under this prefix.
pub(crate) const PAYMENT_INFO_PERSISTENCE_PREFIX: &str = "payments";

pub(crate) struct PaymentInfoStorage<K: Deref>
where
	K::Target: KVStorePersister,
{
	payments: Mutex<HashMap<PaymentHash, PaymentInfo>>,
	persister: K,
}

impl<K: Deref> PaymentInfoStorage<K>
where
	K::Target: KVStorePersister,
{
	pub(crate) fn from_payments(mut payments: Vec<PaymentInfo>, persister: K) -> Self {
		let payments = Mutex::new(HashMap::from_iter(
			payments.drain(..).map(|payment_info| (payment_info.payment_hash, payment_info)),
		));
		Self { payments, persister }
	}

	pub(crate) fn insert(&self, payment_info: PaymentInfo) -> Result<(), Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		let payment_hash = payment_info.payment_hash.clone();
		locked_payments.insert(payment_hash.clone(), payment_info.clone());

		let key = format!(
			"{}/{}",
			PAYMENT_INFO_PERSISTENCE_PREFIX,
			hex_utils::to_string(&payment_hash.0)
		);

		self.persister.persist(&key, &payment_info).map_err(|_| Error::PersistenceFailed)?;

		return Ok(());
	}

	// TODO: Need an `unpersist` method for this?
	//pub(crate) fn remove_payment(&self, payment_hash: &PaymentHash) -> Result<(), Error> {
	//}

	pub(crate) fn get(&self, payment_hash: &PaymentHash) -> Option<PaymentInfo> {
		self.payments.lock().unwrap().get(payment_hash).cloned()
	}

	pub(crate) fn contains(&self, payment_hash: &PaymentHash) -> bool {
		self.payments.lock().unwrap().contains_key(payment_hash)
	}

	pub(crate) fn set_status(
		&self, payment_hash: &PaymentHash, payment_status: PaymentStatus,
	) -> Result<(), Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		if let Some(p) = locked_payments.get_mut(payment_hash) {
			p.status = payment_status;

			let key = format!(
				"{}/{}",
				PAYMENT_INFO_PERSISTENCE_PREFIX,
				hex_utils::to_string(&payment_hash.0)
			);
			self.persister.persist(&key, p).map_err(|_| Error::PersistenceFailed)?;
		}
		Ok(())
	}
}
