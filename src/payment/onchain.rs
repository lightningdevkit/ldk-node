//! Holds a payment handler allowing to send and receive on-chain payments.

use crate::config::Config;
use crate::error::Error;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::types::{ChannelManager, Wallet};

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
	channel_manager: Arc<ChannelManager>,
	config: Arc<Config>,
	logger: Arc<FilesystemLogger>,
}

impl OnchainPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>, wallet: Arc<Wallet>,
		channel_manager: Arc<ChannelManager>, config: Arc<Config>, logger: Arc<FilesystemLogger>,
	) -> Self {
		Self { runtime, wallet, channel_manager, config, logger }
	}

	/// Retrieve a new on-chain/funding address.
	pub fn new_address(&self) -> Result<Address, Error> {
		let funding_address = self.wallet.get_new_address()?;
		log_info!(self.logger, "Generated new funding address: {}", funding_address);
		Ok(funding_address)
	}

	/// Send an on-chain payment to the given address.
	///
	/// This will respect any on-chain reserve we need to keep, i.e., won't allow to cut into
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`].
	///
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`]: crate::BalanceDetails::total_anchor_channels_reserve_sats
	pub fn send_to_address(
		&self, address: &bitcoin::Address, amount_sats: u64,
	) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let spendable_amount_sats =
			self.wallet.get_balances(cur_anchor_reserve_sats).map(|(_, s)| s).unwrap_or(0);

		if spendable_amount_sats < amount_sats {
			log_error!(self.logger,
				"Unable to send payment due to insufficient funds. Available: {}sats, Required: {}sats",
				spendable_amount_sats, amount_sats
			);
			return Err(Error::InsufficientFunds);
		}
		self.wallet.send_to_address(address, Some(amount_sats))
	}

	/// Send an on-chain payment to the given address, draining all the available funds.
	///
	/// This is useful if you have closed all channels and want to migrate funds to another
	/// on-chain wallet.
	///
	/// Please note that this will **not** retain any on-chain reserves, which might be potentially
	/// dangerous if you have open Anchor channels for which you can't trust the counterparty to
	/// spend the Anchor output after channel closure.
	pub fn send_all_to_address(&self, address: &bitcoin::Address) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		self.wallet.send_to_address(address, None)
	}
}
