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
use crate::runtime::Runtime;
use crate::types::PayjoinManager;

/// A payment handler allowing to receive [Payjoin] payments.
///
/// Should be retrieved by calling [`Node::payjoin_payment`].
///
/// [Payjoin]: https://payjoin.org
/// [`Node::payjoin_payment`]: crate::Node::payjoin_payment
pub struct PayjoinPayment {
	runtime: Arc<Runtime>,
	manager: Arc<PayjoinManager>,
	is_running: Arc<RwLock<bool>>,
}

impl PayjoinPayment {
	pub(crate) fn new(
		runtime: Arc<Runtime>, manager: Arc<PayjoinManager>, is_running: Arc<RwLock<bool>>,
	) -> Self {
		Self { runtime, manager, is_running }
	}

	/// Returns a Payjoin URI that can be shared with a sender to receive a Payjoin payment.
	///
	/// The returned string is a BIP 21 URI with Payjoin parameters that the sender can use
	/// to initiate the Payjoin flow.
	pub fn receive(&self, amount: bitcoin::Amount) -> Result<String, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}
		self.runtime.block_on(self.manager.receive_payjoin(amount, None))
	}
}
