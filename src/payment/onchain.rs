// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Holds a payment handler allowing to send and receive on-chain payments.

use std::sync::{Arc, RwLock};

use bitcoin::{Address, Txid};

use crate::config::{AddressType, Config};
use crate::error::Error;
use crate::fee_estimator::ConfirmationTarget;
use crate::logger::{log_info, LdkLogger, Logger};
use crate::types::{ChannelManager, SpendableUtxo, Wallet};
use crate::wallet::{CoinSelectionAlgorithm, OnchainSendAmount};

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

	/// Retrieve a new on-chain address for a specific address type.
	pub fn new_address_for_type(&self, address_type: AddressType) -> Result<Address, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let funding_address = self.wallet.get_new_address_for_type(address_type)?;
		log_info!(
			self.logger,
			"Generated new funding address for {:?}: {}",
			address_type,
			funding_address
		);
		Ok(funding_address)
	}

	/// Returns a list of all UTXOs that are safe to spend.
	///
	/// This excludes any outputs that are currently being used to fund Lightning channels.
	///
	/// **Note:** This does not account for anchor channel reserves. When using these UTXOs
	/// for transactions, ensure you maintain sufficient balance for any required reserves.
	pub fn list_spendable_outputs(&self) -> Result<Vec<SpendableUtxo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		self.wallet
			.get_spendable_utxos(&self.channel_manager)
			.map(|outputs| outputs.into_iter().map(SpendableUtxo::from).collect())
	}

	/// Select UTXOs using a specific coin selection algorithm.
	///
	/// This method allows you to choose which algorithm to use for selecting UTXOs
	/// to meet a target amount. The selected UTXOs will be safe to spend (not funding channels).
	///
	/// # Arguments
	///
	/// * `target_amount_sats` - The target amount in satoshis
	/// * `fee_rate` - The fee rate to use (or None to estimate)
	/// * `algorithm` - The coin selection algorithm to use
	/// * `utxos` - Optional list of UTXO outpoints to select from (or None to use all spendable UTXOs)
	pub fn select_utxos_with_algorithm(
		&self, target_amount_sats: u64, fee_rate: Option<FeeRate>,
		algorithm: CoinSelectionAlgorithm, utxos: Option<Vec<SpendableUtxo>>,
	) -> Result<Vec<SpendableUtxo>, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		// Get available UTXOs, optionally filtering by provided UTXOs
		let available_utxos = match utxos {
			Some(spendable_utxos) => {
				// Get all spendable UTXOs and filter by the provided UTXOs
				let wallet_outputs = self.wallet.get_spendable_utxos(&self.channel_manager)?;
				let outpoint_set: std::collections::HashSet<_> =
					spendable_utxos.iter().map(|u| u.outpoint).collect();
				wallet_outputs
					.into_iter()
					.filter(|output| outpoint_set.contains(&output.outpoint))
					.collect()
			},
			None => self.wallet.get_spendable_utxos(&self.channel_manager)?,
		};

		if available_utxos.is_empty() {
			return Err(Error::InsufficientFunds);
		}

		// Use the set fee_rate or default to fee estimation
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate = maybe_map_fee_rate_opt!(fee_rate)
			.unwrap_or_else(|| self.wallet.estimate_fee_rate(confirmation_target));

		// Get a drain script (change address)
		let drain_script = self.wallet.get_drain_script()?;

		// Apply coin selection
		let selected_outpoints = self.wallet.select_utxos_with_algorithm(
			target_amount_sats,
			available_utxos.clone(),
			fee_rate,
			algorithm,
			&drain_script,
			&self.channel_manager,
		)?;

		// Convert selected outpoints back to SpendableUtxo by direct filtering
		let selected_utxos: Vec<SpendableUtxo> = available_utxos
			.into_iter()
			.filter(|utxo| selected_outpoints.contains(&utxo.outpoint))
			.map(SpendableUtxo::from)
			.collect();

		Ok(selected_utxos)
	}

	/// Calculates the total fee for an on-chain payment without sending it.
	///
	/// This method simulates creating a transaction to the given address for the specified amount
	/// and returns the total fee that would be paid. This is useful for displaying fee estimates
	/// to users before they confirm a transaction.
	///
	/// The calculation respects any on-chain reserve requirements and validates that sufficient
	/// funds are available, just like [`send_to_address`].
	///
	/// **Special handling for maximum amounts:** If the specified amount would result in
	/// insufficient funds due to fees, but is within the spendable balance, this method will
	/// automatically calculate the fee for sending all available funds while retaining the
	/// anchor channel reserve. This allows users to calculate fees when trying to send
	/// their maximum spendable balance.
	///
	/// # Arguments
	///
	/// * `address` - The Bitcoin address to send to
	/// * `amount_sats` - The amount to send in satoshis (or total balance for max send)
	/// * `fee_rate` - Optional fee rate to use (if None, will estimate based on current network conditions)
	/// * `utxos_to_spend` - Optional list of specific UTXOs to use for the transaction
	///
	/// # Returns
	///
	/// The total fee in satoshis that would be paid for this transaction.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::InvalidAddress`] - If the address is invalid
	/// * [`Error::InsufficientFunds`] - If there are insufficient funds for the payment
	/// * [`Error::WalletOperationFailed`] - If fee calculation fails
	///
	/// [`send_to_address`]: Self::send_to_address
	/// [`BalanceDetails::total_onchain_balance_sats`]: crate::balance::BalanceDetails::total_onchain_balance_sats
	pub fn calculate_total_fee(
		&self, address: &bitcoin::Address, amount_sats: u64, fee_rate: Option<FeeRate>,
		utxos_to_spend: Option<Vec<SpendableUtxo>>,
	) -> Result<u64, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);

		// Get current balances
		let (_total_balance, spendable_balance) =
			self.wallet.get_balances(cur_anchor_reserve_sats).unwrap_or((0, 0));

		// First try with the exact amount
		let outpoints = utxos_to_spend.map(|utxos| utxos.into_iter().map(|u| u.outpoint).collect());
		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);

		// Try calculating with exact amount first
		let send_amount =
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats };
		let result = self.wallet.calculate_transaction_fee(
			address,
			send_amount,
			fee_rate_opt,
			outpoints.clone(),
			&self.channel_manager,
		);

		// If we get InsufficientFunds and the amount is within the spendable balance,
		// try calculating as if sending all available funds
		if matches!(result, Err(Error::InsufficientFunds)) && amount_sats <= spendable_balance {
			// Try with AllRetainingReserve to calculate the fee for sending all
			let all_retaining = OnchainSendAmount::AllRetainingReserve { cur_anchor_reserve_sats };
			if let Ok(fee) = self.wallet.calculate_transaction_fee(
				address,
				all_retaining,
				fee_rate_opt,
				outpoints.clone(),
				&self.channel_manager,
			) {
				// Return the fee for sending all available funds
				return Ok(fee);
			}
		}

		result
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
		utxos_to_spend: Option<Vec<SpendableUtxo>>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let cur_anchor_reserve_sats =
			crate::total_anchor_channels_reserve_sats(&self.channel_manager, &self.config);
		let send_amount =
			OnchainSendAmount::ExactRetainingReserve { amount_sats, cur_anchor_reserve_sats };
		let outpoints = utxos_to_spend.map(|utxos| utxos.into_iter().map(|u| u.outpoint).collect());
		let fee_rate_opt = maybe_map_fee_rate_opt!(fee_rate);
		self.wallet.send_to_address(
			address,
			send_amount,
			fee_rate_opt,
			outpoints,
			&self.channel_manager,
		)
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
		self.wallet.send_to_address(address, send_amount, fee_rate_opt, None, &self.channel_manager)
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
		if !*self.is_running.read().unwrap() {
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
		&self, txid: &Txid, fee_rate: Option<FeeRate>, destination_address: Option<Address>,
	) -> Result<Txid, Error> {
		if !*self.is_running.read().unwrap() {
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

	/// Calculates an appropriate fee rate for a CPFP transaction to ensure
	/// the parent transaction gets confirmed within the target number of blocks.
	///
	/// This method analyzes the parent transaction's current fee rate and calculates
	/// how much the child transaction needs to pay to bring the combined package
	/// fee rate up to the target level.
	///
	/// # Arguments
	///
	/// * `parent_txid` - The transaction ID of the parent transaction to accelerate
	/// * `urgent` - If true, uses a more aggressive fee rate for faster confirmation
	///
	/// # Returns
	///
	/// The fee rate that should be used for the child transaction.
	///
	/// # Errors
	///
	/// * [`Error::NotRunning`] - If the node is not running
	/// * [`Error::TransactionNotFound`] - If the parent transaction can't be found
	/// * [`Error::TransactionAlreadyConfirmed`] - If the parent transaction is already confirmed
	/// * [`Error::WalletOperationFailed`] - If fee calculation fails
	pub fn calculate_cpfp_fee_rate(
		&self, parent_txid: &Txid, urgent: bool,
	) -> Result<FeeRate, Error> {
		if !*self.is_running.read().unwrap() {
			return Err(Error::NotRunning);
		}

		let fee_rate = self.wallet.calculate_cpfp_fee_rate(parent_txid, urgent)?;

		#[cfg(not(feature = "uniffi"))]
		{
			Ok(fee_rate)
		}
		#[cfg(feature = "uniffi")]
		{
			Ok(Arc::new(fee_rate))
		}
	}
}
