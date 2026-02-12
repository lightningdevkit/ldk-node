// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Cross-wallet RBF (Replace-By-Fee) transaction construction.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use bdk_wallet::{KeychainKind, PersistedWallet, WalletPersister};
use bitcoin::{Amount, FeeRate, OutPoint, ScriptBuf, Transaction, TxIn, TxOut};

use crate::signing;
use crate::types::{Error, UtxoPsbtInfo};
use crate::utxo::DUST_LIMIT_SATS;

/// Get the value of an input by looking up the referenced UTXO across all
/// wallets.
pub fn get_input_value<K, P>(
	outpoint: &OutPoint, wallets: &HashMap<K, PersistedWallet<P>>,
) -> Result<u64, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	for wallet in wallets.values() {
		if let Some(utxo) = wallet.get_utxo(*outpoint) {
			return Ok(utxo.txout.value.to_sat());
		}

		if let Some(tx_node) = wallet.get_tx(outpoint.txid) {
			if let Some(txout) = tx_node.tx_node.tx.output.get(outpoint.vout as usize) {
				return Ok(txout.value.to_sat());
			}
		}
	}

	Err(Error::UtxoNotFoundLocally(*outpoint))
}

/// Build a cross-wallet RBF replacement transaction.
///
/// Re-uses the original transaction's inputs, optionally adds `extra_utxos`
/// as new inputs, recalculates the fee at `new_fee_rate`, and adjusts the
/// change output accordingly.  Signs with all wallets.
///
/// Pass an empty slice for `extra_utxos` to attempt a change-only bump.
/// If that is insufficient the caller can select additional UTXOs and retry.
pub fn build_cross_wallet_rbf<K, P>(
	wallets: &mut HashMap<K, PersistedWallet<P>>, original_tx: &Transaction, new_fee_rate: FeeRate,
	extra_utxos: &[UtxoPsbtInfo],
) -> Result<Transaction, Error>
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	// Total input value: original inputs + extra inputs.
	let mut total_input_value: u64 = 0;
	for input in &original_tx.input {
		total_input_value += get_input_value(&input.previous_output, wallets)?;
	}
	for info in extra_utxos {
		total_input_value +=
			info.psbt_input.witness_utxo.as_ref().map(|u| u.value.to_sat()).unwrap_or(0);
	}

	// Original fee (for BIP 125 rule 4: replacement fee must be higher).
	let original_output_value: u64 = original_tx.output.iter().map(|o| o.value.to_sat()).sum();
	let original_input_only: u64 = original_tx
		.input
		.iter()
		.map(|i| get_input_value(&i.previous_output, wallets).unwrap_or(0))
		.sum();
	let original_fee = original_input_only.saturating_sub(original_output_value);

	// Build all inputs with RBF signalling.
	let mut new_inputs: Vec<TxIn> = original_tx
		.input
		.iter()
		.map(|txin| TxIn {
			previous_output: txin.previous_output,
			script_sig: ScriptBuf::new(),
			sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
			witness: bitcoin::Witness::default(),
		})
		.collect();
	for info in extra_utxos {
		new_inputs.push(TxIn {
			previous_output: info.outpoint,
			script_sig: ScriptBuf::new(),
			sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
			witness: bitcoin::Witness::default(),
		});
	}

	// Estimate weight with all inputs and calculate required fee.
	let estimated_weight = estimate_tx_weight(wallets, &new_inputs, &original_tx.output);
	let required_fee = new_fee_rate.to_sat_per_kwu() * estimated_weight.to_wu() / 1000;
	let required_fee = std::cmp::max(required_fee, original_fee + 1); // BIP 125 rule 4

	// Find the change output: only adjust an Internal (change) keychain output.
	let mut new_outputs = original_tx.output.clone();
	let change_idx = new_outputs.iter().position(|out| {
		wallets.values().any(|w| {
			w.list_unspent().any(|u| {
				u.keychain == KeychainKind::Internal && u.txout.script_pubkey == out.script_pubkey
			})
		})
	});

	// Recipient value = sum of all non-change outputs.
	let recipient_value: u64 = new_outputs
		.iter()
		.enumerate()
		.filter(|(i, _)| Some(*i) != change_idx)
		.map(|(_, out)| out.value.to_sat())
		.sum();

	// Calculate the new change value from scratch.
	let available_for_change = total_input_value.saturating_sub(required_fee + recipient_value);

	match change_idx {
		Some(idx) => {
			if available_for_change < DUST_LIMIT_SATS {
				log::error!(
					"Insufficient funds for cross-wallet RBF: available_for_change={}, required_fee={}, total_input={}",
					available_for_change,
					required_fee,
					total_input_value
				);
				return Err(Error::InsufficientFunds);
			}
			new_outputs[idx].value = Amount::from_sat(available_for_change);
		},
		None => {
			// No change output (drain tx) — cannot absorb fee increase.
			if total_input_value < required_fee + recipient_value {
				return Err(Error::InsufficientFunds);
			}
			// All surplus goes to fees (no change output to create).
			return Err(Error::OnchainTxCreationFailed);
		},
	}

	let unsigned_tx = Transaction {
		version: original_tx.version,
		lock_time: original_tx.lock_time,
		input: new_inputs,
		output: new_outputs,
	};

	signing::sign_owned_inputs(unsigned_tx, wallets)
}

/// Estimate the weight of a transaction for fee calculation purposes.
fn estimate_tx_weight<K, P>(
	wallets: &HashMap<K, PersistedWallet<P>>, inputs: &[TxIn], outputs: &[TxOut],
) -> bitcoin::Weight
where
	K: Eq + Hash + Copy + Debug,
	P: WalletPersister,
{
	use bitcoin::Weight;

	// Base transaction overhead (conservative estimate):
	// version (4 bytes) + locktime (4 bytes) = 8 non-witness bytes = 32 WU
	// + witness marker & flag (2 bytes) = 2 WU = 34 WU actual.
	// We use 40 WU as a conservative buffer for fee estimation safety.
	let base_weight = Weight::from_wu(40);

	// Input count varint
	let input_count_weight = Weight::from_wu(4); // 1 byte * 4 WU

	// Per-input weight
	let mut input_weight = Weight::ZERO;
	for txin in inputs {
		// 32 bytes txid + 4 bytes vout + 4 bytes sequence + 1 byte script_sig
		// length = 41 bytes * 4 WU
		let base_input = Weight::from_wu(164);

		let satisfaction =
			wallets
				.values()
				.find_map(|w| {
					w.get_utxo(txin.previous_output)
						.map(|utxo| crate::utxo::calculate_utxo_weight(&utxo.txout.script_pubkey))
				})
				.or_else(|| {
					// Fallback for spent UTXOs: look up via the transaction.
					wallets.values().find_map(|w| {
						w.get_tx(txin.previous_output.txid).and_then(|tx_node| {
							tx_node.tx_node.tx.output.get(txin.previous_output.vout as usize).map(
								|txout| crate::utxo::calculate_utxo_weight(&txout.script_pubkey),
							)
						})
					})
				})
				.unwrap_or(Weight::from_wu(272)); // default P2WPKH

		input_weight = input_weight + base_input + satisfaction;
	}

	// Output count varint
	let output_count_weight = Weight::from_wu(4);

	// Per-output weight
	let mut output_weight = Weight::ZERO;
	for txout in outputs {
		// 8 bytes value + 1 byte script length + script_pubkey length, all *4
		let script_len = txout.script_pubkey.len() as u64;
		output_weight += Weight::from_wu((8 + 1 + script_len) * 4);
	}

	base_weight + input_count_weight + input_weight + output_count_weight + output_weight
}
