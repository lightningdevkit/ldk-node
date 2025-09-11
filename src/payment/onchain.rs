// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to send and receive on-chain payments.

use crate::config::Config;
use crate::error::Error;
use crate::logger::{log_info, LdkLogger, Logger};
use crate::types::{ChannelManager, Wallet};
use crate::wallet::OnchainSendAmount;

use bitcoin::{Address, Txid};
use lightning::ln::channelmanager::PaymentId;

use std::sync::{Arc, RwLock};

#[cfg(not(feature = "uniffi"))]
type FeeRate = bitcoin::FeeRate;
#[cfg(feature = "uniffi")]
type FeeRate = Arc<bitcoin::FeeRate>;

macro_rules! maybe_map_fee_rate_opt {
	($fee_rate_opt: expr) => {{
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

/// A payment handler allowing to send and receive on-chain payments.
///
/// Should be retrieved by calling [`Node::onchain_payment`].
///
/// [`Node::onchain_payment`]: crate::Node::onchain_payment
pub struct OnchainPayment {
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	config: Arc<Config>,
	is_running: Arc<RwLock<bool>>,
	logger: Arc<Logger>,
}

impl OnchainPayment {
	pub(crate) fn new(
		wallet: Arc<Wallet>, channel_manager: Arc<ChannelManager>, config: Arc<Config>,
		is_running: Arc<RwLock<bool>>, logger: Arc<Logger>,
	) -> Self {
		Self { wallet, channel_manager, config, is_running, logger }
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
	/// If `fee_rate` is set it will be used on the resulting transaction. Otherwise we'll retrieve
	/// a reasonable estimate from the configured chain source.
	///
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`]: crate::BalanceDetails::total_anchor_channels_reserve_sats
	pub fn send_to_address(
		&self, address: &bitcoin::Address, amount_sats: u64, fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let send_amount =
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats };
		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		self.wallet.send_to_address(address, send_amount, fee_rate_opt)
	}

	/// Send an on-chain payment to the given address, draining the available funds.
	///
	/// This is useful if you have closed all channels and want to migrate funds to another
	/// on-chain wallet.
	///
	/// Please note that if `retain_reserves` is set to `false` this will **not** retain any on-chain reserves, which might be potentially
	/// dangerous if you have open Anchor channels for which you can't trust the counterparty to
	/// spend the Anchor output after channel closure. If `retain_reserves` is set to `true`, this
	/// will try to send all spendable onchain funds, i.e.,
	/// [`BalanceDetails::spendable_onchain_balance_sats`].
	///
	/// If `fee_rate` is set it will be used on the resulting transaction. Otherwise a reasonable
	/// we'll retrieve an estimate from the configured chain source.
	///
	/// [`BalanceDetails::spendable_onchain_balance_sats`]: crate::balance::BalanceDetails::spendable_onchain_balance_sats
	pub fn send_all_to_address(
		&self, address: &bitcoin::Address, retain_reserves: bool, fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let send_amount = if retain_reserves {
			let cur_anchor_reserve_sats =
				crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
			OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats }
		} else {
			OnchainSendAmount::AllDrainingReserve
		};

		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		self.wallet.send_to_address(address, send_amount, fee_rate_opt)
	}

	/// Manually trigger a rebroadcast of a specific transaction according to the default policy.
	///
	/// This is useful if you suspect a transaction may not have propagated properly through the
	/// network and you want to attempt to rebroadcast it immediately rather than waiting for the
	/// automatic background job to handle it.
	///
	/// updating the attempt count and last broadcast time for the transaction in the payment store.
	pub fn rebroadcast_transaction(&self, payment_id: PaymentId) -> Result<(), Error> {
		self.wallet.rebroadcast_transaction(payment_id)
	}

	/// Attempt to bump the fee of an unconfirmed transaction using Replace-by-Fee (RBF).
	///
	/// This creates a new transaction that replaces the original one, increasing the fee by the
	/// specified increment to improve its chances of confirmation. The original transaction must
	/// be signaling RBF replaceability for this to succeed.
	///
	/// The new transaction will have the same outputs as the original but with a
	/// higher fee, resulting in faster confirmation potential.
	///
	/// Returns the Txid of the new replacement transaction if successful.
	pub fn bump_fee_rbf(&self, payment_id: PaymentId) -> Result<Txid, Error> {
		self.wallet.bump_fee_rbf(payment_id)
	}
}
