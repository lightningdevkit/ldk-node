//! Holds a payment handler allowing to send and receive on-chain payments.

use crate::error::Error;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::types::Wallet;

use bitcoin::{Address, Txid};

use std::sync::{Arc, RwLock};

/// A payment handler allowing to send and receive on-chain payments.
///
/// Should be retrieved by calling [`Node::onchain_payment`].
///
/// [`Node::onchain_payment`]: crate::Node::onchain_payment
pub struct OnchainPayment {
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	wallet: Arc<Wallet>,
	logger: Arc<FilesystemLogger>,
}

impl OnchainPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>, wallet: Arc<Wallet>,
		logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { runtime, wallet, logger }
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_address(&self) -> Result<Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	/// Send an on-chain payment to the given address.
	pub fn send_to_address(
		&self, address: &bitcoin::Address, amount_sats: u64,
	) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let cur_balance = self.wallet.get_balance()?;
		if cur_balance.get_spendable() < amount_sats {
			log_error!(self.logger, "Unable to send payment due to insufficient funds.");
			return Err(Error::InsufficientFunds);
		}
		self.wallet.send_to_address(address, Some(amount_sats))
	}

	/// Send an on-chain payment to the given address, draining all the available funds.
	pub fn send_all_to_address(&self, address: &bitcoin::Address) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		self.wallet.send_to_address(address, None)
	}
}
