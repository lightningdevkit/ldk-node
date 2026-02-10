// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! UTXO weight calculation, PSBT preparation, and coin selection helpers.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

#[allow(deprecated)]
use bdk_wallet::coin_selection::CoinSelectionAlgorithm as BdkCoinSelectionAlgorithm;
use bdk_wallet::coin_selection::{
	BranchAndBoundCoinSelection, Excess, LargestFirstCoinSelection, OldestFirstCoinSelection,
	SingleRandomDraw,
};
use bdk_wallet::{LocalOutput, PersistedWallet, WalletPersister, WeightedUtxo};
use bip39::rand::rngs::OsRng;
use bitcoin::{psbt, Amount, FeeRate, OutPoint, Script, ScriptBuf, Weight};

use crate::types::{CoinSelectionAlgorithm, Error, UtxoPsbtInfo};

/// Minimum economical output value (dust limit).
pub const DUST_LIMIT_SATS: u64 = 546;

/// Calculate the satisfaction weight for a UTXO based on its script type.
pub fn calculate_utxo_weight(script_pubkey: &ScriptBuf) -> Weight {
	match script_pubkey.witness_version() {
		Some(bitcoin::WitnessVersion::V0) => Weight::from_wu(272), // P2WPKH
		Some(bitcoin::WitnessVersion::V1) => Weight::from_wu(230), // P2TR
		None => {
			if script_pubkey.is_p2sh() {
				Weight::from_wu(360) // P2SH-wrapped P2WPKH
			} else {
				Weight::from_wu(588) // P2PKH (legacy)
			}
		},
		_ => Weight::from_wu(272), // Fallback to P2WPKH weight
	}
}

/// Prepare UTXO information needed to add local outputs as foreign UTXOs in a
/// cross-wallet PSBT.
pub fn prepare_utxos_for_psbt<K, P>(
	utxos: &[LocalOutput], wallets: &HashMap<K, PersistedWallet<P>>, primary_key: &K,
) -> Result<Vec<UtxoPsbtInfo>, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let mut result = Vec::with_capacity(utxos.len());

	for utxo in utxos {
		let is_primary =
			wallets.get(primary_key).map(|w| w.get_utxo(utxo.outpoint).is_some()).unwrap_or(false);

		let weight = calculate_utxo_weight(&utxo.txout.script_pubkey);

		let mut psbt_input =
			psbt::Input { witness_utxo: Some(utxo.txout.clone()), ..Default::default() };

		// Always include the full previous transaction (non_witness_utxo).
		// BDK requires this for foreign UTXOs during PSBT construction to
		// guard against the SegWit fee vulnerability, even for witness inputs.
		let mut found_tx = false;
		for wallet in wallets.values() {
			if let Some(tx_node) = wallet.get_tx(utxo.outpoint.txid) {
				psbt_input.non_witness_utxo = Some(tx_node.tx_node.tx.as_ref().clone());
				found_tx = true;
				break;
			}
		}

		if !found_tx {
			return Err(Error::UtxoNotFoundLocally(utxo.outpoint));
		}

		result.push(UtxoPsbtInfo { outpoint: utxo.outpoint, psbt_input, weight, is_primary });
	}

	Ok(result)
}

/// Prepare outpoints for PSBT by looking them up across all wallets.
pub fn prepare_outpoints_for_psbt<K, P>(
	outpoints: &[OutPoint], wallets: &HashMap<K, PersistedWallet<P>>, primary_key: &K,
) -> Result<Vec<UtxoPsbtInfo>, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let utxos: Vec<LocalOutput> = outpoints
		.iter()
		.filter_map(|outpoint| {
			wallets.values().find_map(|w| w.get_utxo(*outpoint).filter(|u| !u.is_spent))
		})
		.collect();

	if utxos.len() != outpoints.len() {
		log::error!("Some outpoints were not found in any wallet");
		return Err(Error::WalletOperationFailed);
	}

	prepare_utxos_for_psbt(&utxos, wallets, primary_key)
}

/// Add UTXO information to a BDK `TxBuilder`.
pub fn add_utxos_to_tx_builder<Cs>(
	tx_builder: &mut bdk_wallet::TxBuilder<'_, Cs>, utxo_infos: &[UtxoPsbtInfo],
) -> Result<(), Error> {
	for info in utxo_infos {
		if info.is_primary {
			tx_builder.add_utxo(info.outpoint).map_err(|e| {
				log::error!("Failed to add primary UTXO {:?}: {}", info.outpoint, e);
				Error::OnchainTxCreationFailed
			})?;
		} else {
			tx_builder
				.add_foreign_utxo_with_sequence(
					info.outpoint,
					info.psbt_input.clone(),
					info.weight,
					bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
				)
				.map_err(|e| {
					log::error!("Failed to add foreign UTXO {:?}: {}", info.outpoint, e);
					Error::OnchainTxCreationFailed
				})?;
		}
	}
	Ok(())
}

/// Run coin selection across UTXOs from any wallet.
#[allow(clippy::too_many_arguments)]
pub fn select_utxos_with_algorithm<K, P>(
	target_amount: u64, available_utxos: Vec<LocalOutput>, fee_rate: FeeRate,
	algorithm: CoinSelectionAlgorithm, drain_script: &Script, excluded_outpoints: &[OutPoint],
	wallets: &HashMap<K, PersistedWallet<P>>,
) -> Result<Vec<OutPoint>, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	let safe_utxos: Vec<LocalOutput> = available_utxos
		.into_iter()
		.filter(|utxo| !excluded_outpoints.contains(&utxo.outpoint))
		.collect();

	if safe_utxos.is_empty() {
		log::error!("No spendable UTXOs available after filtering");
		return Err(Error::NoSpendableOutputs);
	}

	let weighted_utxos: Vec<WeightedUtxo> = safe_utxos
		.iter()
		.map(|utxo| {
			// Find the wallet that actually owns this UTXO and use its
			// descriptor for an accurate satisfaction weight.  Falls back to
			// script-type heuristic if no wallet claims the output.
			let satisfaction_weight = wallets
				.values()
				.find_map(|w| {
					if w.get_utxo(utxo.outpoint).is_some() {
						w.public_descriptor(utxo.keychain).max_weight_to_satisfy().ok()
					} else {
						None
					}
				})
				.unwrap_or_else(|| calculate_utxo_weight(&utxo.txout.script_pubkey));

			WeightedUtxo { satisfaction_weight, utxo: bdk_wallet::Utxo::Local(utxo.clone()) }
		})
		.collect();

	let target = Amount::from_sat(target_amount);
	let mut rng = OsRng;

	let result = match algorithm {
		CoinSelectionAlgorithm::BranchAndBound => {
			BranchAndBoundCoinSelection::<SingleRandomDraw>::default().coin_select(
				vec![],
				weighted_utxos,
				fee_rate,
				target,
				drain_script,
				&mut rng,
			)
		},
		CoinSelectionAlgorithm::LargestFirst => LargestFirstCoinSelection.coin_select(
			vec![],
			weighted_utxos,
			fee_rate,
			target,
			drain_script,
			&mut rng,
		),
		CoinSelectionAlgorithm::OldestFirst => OldestFirstCoinSelection.coin_select(
			vec![],
			weighted_utxos,
			fee_rate,
			target,
			drain_script,
			&mut rng,
		),
		CoinSelectionAlgorithm::SingleRandomDraw => SingleRandomDraw.coin_select(
			vec![],
			weighted_utxos,
			fee_rate,
			target,
			drain_script,
			&mut rng,
		),
	}
	.map_err(|e| {
		log::error!("Coin selection failed: {}", e);
		Error::CoinSelectionFailed
	})?;

	if let Excess::Change { amount, .. } = result.excess {
		if amount.to_sat() > 0 && amount.to_sat() < DUST_LIMIT_SATS {
			return Err(Error::CoinSelectionFailed);
		}
	}

	let selected_outputs: Vec<LocalOutput> = result
		.selected
		.into_iter()
		.filter_map(|utxo| match utxo {
			bdk_wallet::Utxo::Local(local) => Some(local),
			_ => None,
		})
		.collect();

	log::debug!(
		"Selected {} UTXOs using {:?} algorithm for target {} sats",
		selected_outputs.len(),
		algorithm,
		target_amount,
	);
	Ok(selected_outputs.into_iter().map(|u| u.outpoint).collect())
}
