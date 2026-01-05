// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler for sending and receiving Payjoin payments.

pub(crate) mod manager;
pub(crate) mod payjoin_session;
pub(crate) mod persist;

use std::sync::{Arc, RwLock};

use crate::error::Error;
use crate::types::PayjoinManager;

#[cfg(not(feature = "uniffi"))]
type FeeRate = bitcoin::FeeRate;
#[cfg(feature = "uniffi")]
type FeeRate = Arc<bitcoin::FeeRate>;

macro_rules! maybe_map_fee_rate_opt {
	($fee_rate_opt:expr) => {{
		#[cfg(not(feature = "uniffi"))]
		{
			$fee_rate_opt
		}
		#[cfg(feature = "uniffi")]
		{
			$fee_rate_opt.map(|f| *f)
		}
	}};
}

/// A payment handler allowing to receive [Payjoin] payments.
///
/// Should be retrieved by calling [`Node::payjoin_payment`].
///
/// [Payjoin]: https://payjoin.org
/// [`Node::payjoin_payment`]: crate::Node::payjoin_payment
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct PayjoinPayment {
	manager: Arc<PayjoinManager>,
	is_running: Arc<RwLock<bool>>,
}

impl PayjoinPayment {
	pub(crate) fn new(manager: Arc<PayjoinManager>, is_running: Arc<RwLock<bool>>) -> Self {
		Self { manager, is_running }
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl PayjoinPayment {
	/// Returns a Payjoin URI that can be shared with a sender to receive a Payjoin payment.
	///
	/// The returned string is a BIP 21 URI with Payjoin parameters that the sender can use
	/// to initiate the Payjoin flow.
	pub async fn receive(
		&self, amount_sats: u64, fee_rate: Option<FeeRate>,
	) -> Result<String, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		self.manager.receive_payjoin(amount_sats, fee_rate_opt).await
	}
}
