// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2025 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Structs from the Esplora API
//!
//! See: <https://github.com/Blockstream/esplora/blob/master/API.md>

use bitcoin::hash_types;
use serde::Deserialize;
use std::collections::HashMap;

pub use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hash_types::TxMerkleNode;
pub use bitcoin::hex::FromHex;
pub use bitcoin::{
	absolute, block, transaction, Address, Amount, Block, BlockHash, CompactTarget, FeeRate,
	OutPoint, Script, ScriptBuf, ScriptHash, Transaction, TxIn, TxOut, Txid, Weight, Witness,
	Wtxid,
};

/// Information about a previous output.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PrevOut {
	/// The value of the previous output, in satoshis.
	pub value: u64,
	/// The ScriptPubKey that the previous output is locked to, as a [`ScriptBuf`].
	pub scriptpubkey: ScriptBuf,
}

/// Information about an input from a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Vin {
	/// The [`Txid`] of the previous [`Transaction`] this input spends from.
	pub txid: Txid,
	/// The output index of the previous output in the [`Transaction`] that created it.
	pub vout: u32,
	/// The previous output amount and ScriptPubKey.
	/// `None` if this is a coinbase input.
	pub prevout: Option<PrevOut>,
	/// The ScriptSig authorizes spending this input.
	pub scriptsig: ScriptBuf,
	/// The Witness that authorizes spending this input, if this is a SegWit spend.
	#[serde(deserialize_with = "deserialize_witness", default)]
	pub witness: Vec<Vec<u8>>,
	/// The sequence value for this input, used to set RBF and Locktime behavior.
	pub sequence: u32,
	/// Whether this is a coinbase input.
	pub is_coinbase: bool,
}

/// Information about a [`Transaction`]s output.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Vout {
	/// The value of the output, in satoshis.
	pub value: u64,
	/// The ScriptPubKey that the output is locked to, as a [`ScriptBuf`].
	pub scriptpubkey: ScriptBuf,
}

/// The confirmation status of a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TxStatus {
	/// Whether the [`Transaction`] is confirmed or not.
	pub confirmed: bool,
	/// The block height the [`Transaction`] was confirmed in.
	pub block_height: Option<u32>,
	/// The [`BlockHash`] of the block the [`Transaction`] was confirmed in.
	pub block_hash: Option<BlockHash>,
	/// The time that the block was mined at, as a UNIX timestamp.
	/// Note: this timestamp is set by the miner and may not reflect the exact time of mining.
	pub block_time: Option<u64>,
}

/// A Merkle inclusion proof for a transaction, given it's [`Txid`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MerkleProof {
	/// The height of the block the [`Transaction`] was confirmed in.
	pub block_height: u32,
	/// A list of transaction hashes the current hash is paired with,
	/// recursively, in order to trace up to obtain the Merkle root of the
	/// [`Block`], deepest pairing first.
	pub merkle: Vec<Txid>,
	/// The 0-based index of the position of the [`Transaction`] in the
	/// ordered list of [`Transaction`]s in the [`Block`].
	pub pos: usize,
}

/// The spend status of a [`TxOut`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OutputStatus {
	/// Whether the [`TxOut`] is spent or not.
	pub spent: bool,
	/// The [`Txid`] that spent this [`TxOut`].
	pub txid: Option<Txid>,
	/// The input index of this [`TxOut`] in the [`Transaction`] that spent it.
	pub vin: Option<u64>,
	/// Information about the [`Transaction`] that spent this [`TxOut`].
	pub status: Option<TxStatus>,
}

/// Information about a [`Block`]s status.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockStatus {
	/// Whether this [`Block`] belongs to the chain with the most
	/// Proof-of-Work (false for [`Block`]s that belong to a stale chain).
	pub in_best_chain: bool,
	/// The height of this [`Block`].
	pub height: Option<u32>,
	/// The [`BlockHash`] of the [`Block`] that builds on top of this one.
	pub next_best: Option<BlockHash>,
}

/// A [`Transaction`] in the format returned by Esplora.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Tx {
	/// The [`Txid`] of the [`Transaction`].
	pub txid: Txid,
	/// The version number of the [`Transaction`].
	pub version: i32,
	/// The locktime of the [`Transaction`].
	/// Sets a time or height after which the [`Transaction`] can be mined.
	pub locktime: u32,
	/// The array of inputs in the [`Transaction`].
	pub vin: Vec<Vin>,
	/// The array of outputs in the [`Transaction`].
	pub vout: Vec<Vout>,
	/// The [`Transaction`] size in raw bytes (NOT virtual bytes).
	pub size: usize,
	/// The [`Transaction`]'s weight units.
	pub weight: u64,
	/// The confirmation status of the [`Transaction`].
	pub status: TxStatus,
	/// The fee amount paid by the [`Transaction`], in satoshis.
	pub fee: u64,
}

/// Information about a bitcoin [`Block`].
#[derive(Debug, Clone, Deserialize)]
pub struct BlockInfo {
	/// The [`Block`]'s [`BlockHash`].
	pub id: BlockHash,
	/// The [`Block`]'s height.
	pub height: u32,
	/// The [`Block`]'s version.
	pub version: block::Version,
	/// The [`Block`]'s UNIX timestamp.
	pub timestamp: u64,
	/// The [`Block`]'s [`Transaction`] count.
	pub tx_count: u64,
	/// The [`Block`]'s size, in bytes.
	pub size: usize,
	/// The [`Block`]'s weight.
	pub weight: u64,
	/// The Merkle root of the transactions in the block.
	pub merkle_root: hash_types::TxMerkleNode,
	/// The [`BlockHash`] of the previous [`Block`] (`None` for the genesis block).
	pub previousblockhash: Option<BlockHash>,
	/// The [`Block`]'s MTP (Median Time Past).
	pub mediantime: u64,
	/// The [`Block`]'s nonce value.
	pub nonce: u32,
	/// The [`Block`]'s `bits` value as a [`CompactTarget`].
	pub bits: CompactTarget,
	/// The [`Block`]'s difficulty target value.
	pub difficulty: f64,
}

impl PartialEq for BlockInfo {
	fn eq(&self, other: &Self) -> bool {
		let Self { difficulty: d1, .. } = self;
		let Self { difficulty: d2, .. } = other;

		self.id == other.id
			&& self.height == other.height
			&& self.version == other.version
			&& self.timestamp == other.timestamp
			&& self.tx_count == other.tx_count
			&& self.size == other.size
			&& self.weight == other.weight
			&& self.merkle_root == other.merkle_root
			&& self.previousblockhash == other.previousblockhash
			&& self.mediantime == other.mediantime
			&& self.nonce == other.nonce
			&& self.bits == other.bits
			&& ((d1.is_nan() && d2.is_nan()) || (d1 == d2))
	}
}
impl Eq for BlockInfo {}

/// Time-related information about a [`Block`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockTime {
	/// The [`Block`]'s timestamp.
	pub timestamp: u64,
	/// The [`Block`]'s height.
	pub height: u32,
}

/// Summary about a [`Block`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BlockSummary {
	/// The [`Block`]'s hash.
	pub id: BlockHash,
	/// The [`Block`]'s timestamp and height.
	#[serde(flatten)]
	pub time: BlockTime,
	/// The [`BlockHash`] of the previous [`Block`] (`None` for the genesis [`Block`]).
	pub previousblockhash: Option<BlockHash>,
	/// The Merkle root of the [`Block`]'s [`Transaction`]s.
	pub merkle_root: TxMerkleNode,
}

/// Statistics about an [`Address`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AddressStats {
	/// The [`Address`].
	pub address: String,
	/// The summary of confirmed [`Transaction`]s for this [`Address`].
	pub chain_stats: AddressTxsSummary,
	/// The summary of mempool [`Transaction`]s for this [`Address`].
	pub mempool_stats: AddressTxsSummary,
}

/// A summary of [`Transaction`]s in which an [`Address`] was involved.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct AddressTxsSummary {
	/// The number of funded [`TxOut`]s.
	pub funded_txo_count: u32,
	/// The sum of the funded [`TxOut`]s, in satoshis.
	pub funded_txo_sum: u64,
	/// The number of spent [`TxOut`]s.
	pub spent_txo_count: u32,
	/// The sum of the spent [`TxOut`]s, in satoshis.
	pub spent_txo_sum: u64,
	/// The total number of [`Transaction`]s.
	pub tx_count: u32,
}

/// Statistics about a particular [`Script`] hash's confirmed and mempool transactions.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct ScriptHashStats {
	/// The summary of confirmed [`Transaction`]s for this [`Script`] hash.
	pub chain_stats: ScriptHashTxsSummary,
	/// The summary of mempool [`Transaction`]s for this [`Script`] hash.
	pub mempool_stats: ScriptHashTxsSummary,
}

/// Contains a summary of the [`Transaction`]s for a particular [`Script`] hash.
pub type ScriptHashTxsSummary = AddressTxsSummary;

/// Information about a [`TxOut`]'s status: confirmation status,
/// confirmation height, confirmation block hash and confirmation block time.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct UtxoStatus {
	/// Whether or not the [`TxOut`] is confirmed.
	pub confirmed: bool,
	/// The block height in which the [`TxOut`] was confirmed.
	pub block_height: Option<u32>,
	/// The block hash in which the [`TxOut`] was confirmed.
	pub block_hash: Option<BlockHash>,
	/// The UNIX timestamp in which the [`TxOut`] was confirmed.
	pub block_time: Option<u64>,
}

/// Information about an [`TxOut`]'s outpoint, confirmation status and value.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct Utxo {
	/// The [`Txid`] of the [`Transaction`] that created the [`TxOut`].
	pub txid: Txid,
	/// The output index of the [`TxOut`] in the [`Transaction`] that created it.
	pub vout: u32,
	/// The confirmation status of the [`TxOut`].
	pub status: UtxoStatus,
	/// The value of the [`TxOut`] as an [`Amount`].
	pub value: Amount,
}

/// Statistics about the mempool.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MempoolStats {
	/// The number of [`Transaction`]s in the mempool.
	pub count: usize,
	/// The total size of mempool [`Transaction`]s, in virtual bytes.
	pub vsize: usize,
	/// The total fee paid by mempool [`Transaction`]s, in satoshis.
	pub total_fee: u64,
	/// The mempool's fee rate distribution histogram.
	///
	/// An array of `(feerate, vsize)` tuples, where each entry's `vsize` is the total vsize
	/// of [`Transaction`]s paying more than `feerate` but less than the previous entry's `feerate`
	/// (except for the first entry, which has no upper bound).
	pub fee_histogram: Vec<(f64, usize)>,
}

/// A [`Transaction`] that recently entered the mempool.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MempoolRecentTx {
	/// The [`Transaction`]'s ID, as a [`Txid`].
	pub txid: Txid,
	/// The [`Amount`] of fees paid by the transaction, in satoshis.
	pub fee: u64,
	/// The [`Transaction`]'s size, in virtual bytes.
	pub vsize: usize,
	/// Combined [`Amount`] of the [`Transaction`], in satoshis.
	pub value: u64,
}

/// The result for a broadcasted package of [`Transaction`]s.
#[derive(Deserialize, Debug)]
pub struct SubmitPackageResult {
	/// The transaction package result message. "success" indicates all transactions were accepted
	/// into or are already in the mempool.
	pub package_msg: String,
	/// Transaction results keyed by [`Wtxid`].
	#[serde(rename = "tx-results")]
	pub tx_results: HashMap<Wtxid, TxResult>,
	/// List of txids of replaced transactions.
	#[serde(rename = "replaced-transactions")]
	pub replaced_transactions: Option<Vec<Txid>>,
}

/// The result [`Transaction`] for a broadcasted package of [`Transaction`]s.
#[derive(Deserialize, Debug)]
pub struct TxResult {
	/// The transaction id.
	pub txid: Txid,
	/// The [`Wtxid`] of a different transaction with the same [`Txid`] but different witness found
	/// in the mempool.
	///
	/// If set, this means the submitted transaction was ignored.
	#[serde(rename = "other-wtxid")]
	pub other_wtxid: Option<Wtxid>,
	/// Sigops-adjusted virtual transaction size.
	pub vsize: Option<u32>,
	/// Transaction fees.
	pub fees: Option<MempoolFeesSubmitPackage>,
	/// The transaction error string, if it was rejected by the mempool
	pub error: Option<String>,
}

/// The mempool fees for a resulting [`Transaction`] broadcasted by a package of [`Transaction`]s.
#[derive(Deserialize, Debug)]
pub struct MempoolFeesSubmitPackage {
	/// Transaction fee.
	#[serde(with = "bitcoin::amount::serde::as_btc")]
	pub base: Amount,
	/// The effective feerate.
	///
	/// Will be `None` if the transaction was already in the mempool. For example, the package
	/// feerate and/or feerate with modified fees from the `prioritisetransaction` JSON-RPC method.
	#[serde(rename = "effective-feerate", default, deserialize_with = "deserialize_feerate")]
	pub effective_feerate: Option<FeeRate>,
	/// If [`Self::effective_feerate`] is provided, this holds the [`Wtxid`]s of the transactions
	/// whose fees and vsizes are included in effective-feerate.
	#[serde(rename = "effective-includes")]
	pub effective_includes: Option<Vec<Wtxid>>,
}

impl Tx {
	/// Convert a transaction from the format returned by Esplora into a `rust-bitcoin`
	/// [`Transaction`].
	pub fn to_tx(&self) -> Transaction {
		Transaction {
			version: transaction::Version::non_standard(self.version),
			lock_time: bitcoin::absolute::LockTime::from_consensus(self.locktime),
			input: self
				.vin
				.iter()
				.cloned()
				.map(|vin| TxIn {
					previous_output: OutPoint { txid: vin.txid, vout: vin.vout },
					script_sig: vin.scriptsig,
					sequence: bitcoin::Sequence(vin.sequence),
					witness: Witness::from_slice(&vin.witness),
				})
				.collect(),
			output: self
				.vout
				.iter()
				.cloned()
				.map(|vout| TxOut {
					value: Amount::from_sat(vout.value),
					script_pubkey: vout.scriptpubkey,
				})
				.collect(),
		}
	}

	/// Get the confirmation time from a [`Tx`].
	pub fn confirmation_time(&self) -> Option<BlockTime> {
		match self.status {
			TxStatus {
				confirmed: true,
				block_height: Some(height),
				block_time: Some(timestamp),
				..
			} => Some(BlockTime { timestamp, height }),
			_ => None,
		}
	}

	/// Get a list of the [`Tx`]'s previous outputs.
	pub fn previous_outputs(&self) -> Vec<Option<TxOut>> {
		self.vin
			.iter()
			.cloned()
			.map(|vin| {
				vin.prevout.map(|po| TxOut {
					script_pubkey: po.scriptpubkey,
					value: Amount::from_sat(po.value),
				})
			})
			.collect()
	}

	/// Get the weight of a [`Tx`].
	pub fn weight(&self) -> Weight {
		Weight::from_wu(self.weight)
	}

	/// Get the fee paid by a [`Tx`].
	pub fn fee(&self) -> Amount {
		Amount::from_sat(self.fee)
	}
}

fn deserialize_witness<'de, D>(d: D) -> Result<Vec<Vec<u8>>, D::Error>
where
	D: serde::de::Deserializer<'de>,
{
	let list = Vec::<String>::deserialize(d)?;
	list.into_iter()
		.map(|hex_str| Vec::<u8>::from_hex(&hex_str))
		.collect::<Result<Vec<Vec<u8>>, _>>()
		.map_err(serde::de::Error::custom)
}

fn deserialize_feerate<'de, D>(d: D) -> Result<Option<FeeRate>, D::Error>
where
	D: serde::de::Deserializer<'de>,
{
	use serde::de::Error;

	let btc_per_kvb = match Option::<f64>::deserialize(d)? {
		Some(v) => v,
		None => return Ok(None),
	};
	let sat_per_kwu = btc_per_kvb * 25_000_000.0;
	if sat_per_kwu.is_infinite() {
		return Err(D::Error::custom("feerate overflow"));
	}
	Ok(Some(FeeRate::from_sat_per_kwu(sat_per_kwu as u64)))
}
