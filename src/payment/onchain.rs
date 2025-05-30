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
	runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>,
	wallet: Arc<Wallet>,
	channel_manager: Arc<ChannelManager>,
	config: Arc<Config>,
	logger: Arc<Logger>,
}

impl OnchainPayment {
	pub(crate) fn new(
		runtime: Arc<RwLock<Option<Arc<tokio::runtime::Runtime>>>>, wallet: Arc<Wallet>,
		channel_manager: Arc<ChannelManager>, config: Arc<Config>, logger: Arc<Logger>,
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
	/// If `fee_rate` is set it will be used on the resulting transaction. Otherwise we'll retrieve
	/// a reasonable estimate from the configured chain source.
	///
	/// [`BalanceDetails::total_anchor_channels_reserve_sats`]: crate::BalanceDetails::total_anchor_channels_reserve_sats
	pub fn send_to_address(
		&self, address: &bitcoin::Address, amount_sats: u64, fee_rate: Option<FeeRate>,
	) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
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
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
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

	/// Bumps the fee of an existing transaction using Replace-By-Fee (RBF).
	///
	/// This allows a previously sent transaction to be replaced with a new version
	/// that pays a higher fee. The original transaction must have been created with
	/// RBF enabled (which is the default for transactions created by LDK).
	///
	/// **Note:** This cannot be used on funding transactions as doing so would invalidate the channel.
	///
	/// # Arguments
	///
	/// * `txid` - The transaction ID of the transaction to be replaced
	/// * `fee_rate` - The new fee rate to use (must be higher than the original fee rate)
	///
	/// # Returns
	///
	/// The transaction ID of the new transaction if successful.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the transaction can't be found in the wallet
	/// * [`Error::TransactionAlreadyConfirmed`] - If the transaction is already confirmed
	/// * [`Error::CannotRbfFundingTransaction`] - If the transaction is a channel funding transaction
	/// * [`Error::InvalidFeeRate`] - If the new fee rate is not higher than the original
	/// * [`Error::OnchainTxCreationFailed`] - If the new transaction couldn't be created
	pub fn bump_fee_by_rbf(&self, txid: &Txid, fee_rate: FeeRate) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		// Pass through to the wallet implementation
		#[cfg(not(feature = "uniffi"))]
		let fee_rate_param = fee_rate;
		#[cfg(feature = "uniffi")]
		let fee_rate_param = *fee_rate;

		self.wallet.bump_fee_by_rbf(txid, fee_rate_param, &self.channel_manager)
	}

	/// Accelerates confirmation of a transaction using Child-Pays-For-Parent (CPFP).
	///
	/// This creates a new transaction (child) that spends an output from the
	/// transaction to be accelerated (parent), with a high enough fee to pay for both.
	///
	/// # Arguments
	///
	/// * `txid` - The transaction ID of the transaction to be accelerated
	/// * `fee_rate` - The fee rate to use for the child transaction (or None to calculate automatically)
	/// * `destination_address` - Optional address to send the funds to (if None, funds are sent to an internal address)
	///
	/// # Returns
	///
	/// The transaction ID of the child transaction if successful.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the transaction can't be found
	/// * [`Error::TransactionAlreadyConfirmed`] - If the transaction is already confirmed
	/// * [`Error::NoSpendableOutputs`] - If the transaction has no spendable outputs
	/// * [`Error::OnchainTxCreationFailed`] - If the child transaction couldn't be created
	pub fn accelerate_by_cpfp(
		&self,
		txid: &Txid,
		fee_rate: Option<FeeRate>,
		destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		let rt_lock = self.runtime.read().unwrap();
		if rt_lock.is_none() {
			return Err(Error::NotRunning);
		}

		// Calculate fee rate if not provided
		#[cfg(not(feature = "uniffi"))]
		let fee_rate_param = match fee_rate {
			Some(rate) => rate,
			None => self.wallet.calculate_cpfp_fee_rate(txid, true)?,
		};

		#[cfg(feature = "uniffi")]
		let fee_rate_param = match fee_rate {
			Some(rate) => *rate,
			None => self.wallet.calculate_cpfp_fee_rate(txid, true)?,
		};

		// Pass through to the wallet implementation
		self.wallet.accelerate_by_cpfp(txid, fee_rate_param, destination_address)
	}
}