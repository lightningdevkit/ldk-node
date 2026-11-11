// SPDX-License-Identifier: MIT OR Apache-2.0

//! # Esplora API
//!
//! This module implements the types and deserializers
//! needed to interact with an Esplora-compliant server.
//!
//! Refer to the [Esplora API] specification for the complete API reference.
//!
//! [Esplora API]: <https://github.com/Blockstream/esplora/blob/master/API.md>

use serde::Deserialize;
use std::collections::HashMap;

pub use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hash_types;
use bitcoin::hash_types::TxMerkleNode;
pub use bitcoin::hex::FromHex;
pub use bitcoin::{
	absolute, block, transaction, Address, Amount, Block, BlockHash, CompactTarget, FeeRate,
	OutPoint, Script, ScriptBuf, ScriptHash, Sequence, Transaction, TxIn, TxOut, Txid, Weight,
	Witness, Wtxid,
};

/// An input to a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Vin {
	/// The [`Txid`] of the [`Transaction`] that created this output.
	pub txid: Txid,
	/// The index of this output in the [`Transaction`] that created it.
	pub vout: u32,
	/// This input's previous output [`Amount`] and [script pubkey][Script].
	///
	/// `None` if this input spends a coinbase output.
	pub prevout: Option<Vout>,
	/// The [`Script`] that unlocks this input.
	pub scriptsig: ScriptBuf,
	/// The [`Witness`] that unlocks this input.
	#[serde(default)]
	pub witness: Witness,
	/// The sequence value for this input.
	pub sequence: Sequence,
	/// Whether this is a coinbase input.
	pub is_coinbase: bool,
}

/// An output from a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Vout {
	/// The output's [`Amount`].
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub value: Amount,
	/// The [script pubkey][Script] this output is locked to.
	pub scriptpubkey: ScriptBuf,
}

/// The confirmation status of a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TxStatus {
	/// Whether the [`Transaction`] is confirmed or not.
	pub confirmed: bool,
	/// The block height that confirmed the [`Transaction`].
	pub block_height: Option<u32>,
	/// The [`BlockHash`] of the block that confirmed the [`Transaction`].
	///
	/// `None` if the [`Transaction`] was confirmed by the genesis block.
	pub block_hash: Option<BlockHash>,
	/// The UNIX timestamp of the block that confirmed the [`Transaction`], as claimed by the
	/// miner.
	pub block_time: Option<u64>,
}

/// A Merkle inclusion proof for a [`Transaction`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MerkleProof {
	/// The block height that confirmed the [`Transaction`].
	pub block_height: u32,
	/// The Merkle proof of inclusion of a [`Transaction`] in a [`Block`].
	///
	/// Elements are returned left-to-right and bottom-to-top.
	pub merkle: Vec<Txid>,
	/// The 0-indexed position of the [`Transaction`] in the [`Block`].
	pub pos: usize,
}

/// The status of a [`TxOut`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OutputStatus {
	/// Whether the [`TxOut`] is spent.
	pub spent: bool,
	/// The [`Txid`] of the [`Transaction`] that spent this [`TxOut`].
	pub txid: Option<Txid>,
	/// The input index of this [`TxOut`] in the [`Transaction`] that spent it.
	pub vin: Option<u64>,
	/// Information about the [`Transaction`] that spent this [`TxOut`].
	pub status: Option<TxStatus>,
}

/// The status of a [`Block`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockStatus {
	/// Whether this [`Block`] belongs to the chain with the most Proof-of-Work.
	pub in_best_chain: bool,
	/// The height of this [`Block`].
	pub height: Option<u32>,
	/// The [`BlockHash`] of the [`Block`] that builds on top of this [`Block`].
	pub next_best: Option<BlockHash>,
}

/// A transaction in the format returned by Esplora.
///
/// Unlike the native `rust-bitcoin` [`Transaction`], [`EsploraTx`]
/// includes additional metadata such as the [`TxStatus`], transaction fee,
/// and transaction [`Weight`], as indexed and reported by Esplora servers.
///
/// To convert it into a [`Transaction`], use [`EsploraTx::to_tx`] or `.into()`.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EsploraTx {
	/// The [`Txid`] of the [`Transaction`].
	pub txid: Txid,
	/// The version of the [`Transaction`].
	pub version: transaction::Version,
	/// The locktime of the [`Transaction`].
	/// Sets a time or height after which the [`Transaction`] can be mined.
	pub locktime: absolute::LockTime,
	/// The array of inputs in the [`Transaction`].
	pub vin: Vec<Vin>,
	/// The array of outputs in the [`Transaction`].
	pub vout: Vec<Vout>,
	/// The [`Transaction`] size in raw bytes (NOT virtual bytes).
	pub size: usize,
	/// The [`Transaction`]'s weight.
	pub weight: Weight,
	/// The confirmation status of the [`Transaction`].
	pub status: TxStatus,
	/// The fee paid by the [`Transaction`], in satoshis.
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub fee: Amount,
}

/// A summary of a [`Block`].
///
/// Contains additional metadata about a [`Block`], but not the whole [`Block`].
///
/// For the complete [`Block`] contents, use the `get_block_by_hash` client method.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockInfo {
	/// The [`BlockHash`] of this [`Block`].
	pub id: BlockHash,
	/// The block height of this [`Block`].
	pub height: u32,
	/// The version of this [`Block`].
	pub version: block::Version,
	/// The UNIX timestamp of this [`Block`], as claimed by the miner.
	pub timestamp: u64,
	/// The [`Transaction`] count for this [`Block`].
	pub tx_count: u64,
	/// The size of this [`Block`], in bytes.
	pub size: usize,
	/// The [`Weight`] of this [`Block`].
	pub weight: Weight,
	/// The Merkle root of this [`Block`].
	pub merkle_root: hash_types::TxMerkleNode,
	/// The [`BlockHash`] of the previous [`Block`].
	///
	/// `None` for the genesis block.
	pub previousblockhash: Option<BlockHash>,
	/// The Median Time Past value for this [`Block`].
	pub mediantime: u64,
	/// This [`Block`]'s nonce.
	pub nonce: u32,
	/// The [`Block`]'s `bits` value, encoded as a [`CompactTarget`].
	pub bits: CompactTarget,
	/// The [`Block`]'s difficulty target value.
	pub difficulty: f64,
}

/// A manual `PartialEq` implementation is required
/// since [`BlockInfo::difficulty`] is an `f64`.
///
/// This treats two `NaN` difficulty values as equal,
/// allowing [`BlockInfo`] to implement [`Eq`] correctly.
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

/// The UNIX timestamp and height of a [`Block`].
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockTime {
	/// The UNIX timestamp of the [`Block`], as claimed by the miner.
	pub timestamp: u64,
	/// The block height of the [`Block`].
	pub height: u32,
}

/// Summary about a [`Block`].
#[allow(deprecated)]
#[deprecated(since = "0.13.0", note = "use `BlockInfo` instead")]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct BlockSummary {
	/// The [`BlockHash`] of the [`Block`].
	pub id: BlockHash,
	/// The UNIX timestamp and height of the [`Block`].
	#[serde(flatten)]
	pub time: BlockTime,
	/// The [`BlockHash`] of the previous [`Block`].
	///
	/// `None` for the genesis block.
	pub previousblockhash: Option<BlockHash>,
	/// The Merkle root of this [`Block`].
	pub merkle_root: TxMerkleNode,
}

/// Statistics about an [`Address`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AddressStats {
	/// The [`Address`].
	#[serde(deserialize_with = "deserialize_address_assume_checked")]
	pub address: Address,
	/// The summary of confirmed [`Transaction`]s for this [`Address`].
	pub chain_stats: AddressTxsSummary,
	/// The summary of unconfirmed mempool [`Transaction`]s for this [`Address`].
	pub mempool_stats: AddressTxsSummary,
}

/// A summary of [`Transaction`]s in which an [`Address`] is involved.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct AddressTxsSummary {
	/// The current number of funded [`TxOut`]s for this [`Address`].
	pub funded_txo_count: u32,
	/// The total [`Amount`] of funded [`TxOut`]s for this [`Address`].
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub funded_txo_sum: Amount,
	/// The number of spent [`TxOut`]s for this [`Address`].
	pub spent_txo_count: u32,
	/// The total [`Amount`] of spent [`TxOut`]s for this [`Address`].
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub spent_txo_sum: Amount,
	/// The total number of [`Transaction`]s for this [`Address`].
	pub tx_count: u32,
}

/// Statistics about a [scripthash](Script)'s confirmed and mempool [`Transaction`]s.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct ScriptHashStats {
	/// The summary of confirmed [`Transaction`]s for this [scripthash](Script).
	pub chain_stats: ScriptHashTxsSummary,
	/// The summary of mempool [`Transaction`]s for this [scripthash](Script).
	pub mempool_stats: ScriptHashTxsSummary,
}

/// A summary of [`Transaction`]s for a particular [scripthash](Script).
pub type ScriptHashTxsSummary = AddressTxsSummary;

/// The confirmation status of a [`TxOut`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct UtxoStatus {
	/// Whether the [`TxOut`] is confirmed.
	pub confirmed: bool,
	/// The block height in which the [`TxOut`] was confirmed.
	pub block_height: Option<u32>,
	/// The block hash in which the [`TxOut`] was confirmed.
	pub block_hash: Option<BlockHash>,
	/// The UNIX timestamp in which the [`TxOut`] was confirmed, as reported by the miner.
	pub block_time: Option<u64>,
}

/// An unspent [`TxOut`], including its outpoint, confirmation status and value.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize)]
pub struct Utxo {
	/// The [`Txid`] of the [`Transaction`] that created this [`TxOut`].
	pub txid: Txid,
	/// The output index of this [`TxOut`] in the [`Transaction`] that created it.
	pub vout: u32,
	/// The confirmation status of this [`TxOut`].
	pub status: UtxoStatus,
	/// The value of this [`TxOut`].
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub value: Amount,
}

/// Statistics about the mempool.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MempoolStats {
	/// The number of [`Transaction`]s currently in the mempool.
	pub count: usize,
	/// The total size of mempool [`Transaction`]s, in virtual bytes.
	pub vsize: usize,
	/// The total fee paid by mempool [`Transaction`]s.
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub total_fee: Amount,
	/// The mempool's fee rate distribution histogram.
	///
	/// An array of `(feerate, vsize)` tuples, where each entry's
	/// `vsize` is the total vsize of [`Transaction`]s paying more
	/// than `feerate` but less than the previous entry's `feerate`
	/// (except for the first entry, which has no upper bound).
	///
	/// The Esplora API reports `vsize` in virtual bytes. This field
	/// currently stores that raw value in [`Weight`].
	#[serde(deserialize_with = "deserialize_fee_histogram")]
	pub fee_histogram: Vec<(FeeRate, Weight)>,
}

/// A [`Transaction`] that recently entered the mempool.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MempoolRecentTx {
	/// The [`Transaction`]'s [`Txid`].
	pub txid: Txid,
	/// The fee paid by the [`Transaction`].
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub fee: Amount,
	/// The [`Transaction`]'s size, in virtual bytes.
	pub vsize: usize,
	/// The combined value of the [`Transaction`]'s outputs.
	#[serde(with = "bitcoin::amount::serde::as_sat")]
	pub value: Amount,
}

/// The global result of a [`Transaction`] package submission.
#[derive(Deserialize, Debug)]
pub struct SubmitPackageResult {
	/// The [`Transaction`] package result message.
	///
	/// "success" indicates all transactions were
	/// accepted or are already in the mempool.
	pub package_msg: String,
	/// The list of individual [`Transaction`] broadcast
	/// results, keyed by each [`Transaction`]'s [`Wtxid`].
	#[serde(rename = "tx-results")]
	pub tx_results: HashMap<Wtxid, TxResult>,
	/// The list of [`Txid`]s of replaced [`Transaction`]s.
	#[serde(rename = "replaced-transactions")]
	pub replaced_transactions: Option<Vec<Txid>>,
}

/// A per-transaction result of a [`Transaction`] package submission.
#[derive(Deserialize, Debug)]
pub struct TxResult {
	/// The [`Transaction`]'s [`Txid`].
	pub txid: Txid,
	/// The [`Wtxid`] of a different [`Transaction`] with the same [`Txid`],
	/// but different Witness found in the mempool.
	///
	/// If `Some`, means the submitted [`Transaction`] was ignored.
	#[serde(rename = "other-wtxid")]
	pub other_wtxid: Option<Wtxid>,
	/// `sigops`-adjusted transaction size, in virtual bytes.
	pub vsize: Option<u32>,
	/// The effective fee paid by the [`Transaction`].
	pub fees: Option<MempoolFeesSubmitPackage>,
	/// The [`Transaction`] submission error string.
	pub error: Option<String>,
}

/// The fees for a [`Transaction`] submitted as part of a package.
#[derive(Deserialize, Debug)]
pub struct MempoolFeesSubmitPackage {
	/// The base fee paid by the [`Transaction`].
	#[serde(with = "bitcoin::amount::serde::as_btc")]
	pub base: Amount,
	/// The effective feerate paid by this [`Transaction`].
	///
	/// Is `None` if the transaction was already in the mempool.
	#[serde(rename = "effective-feerate", default, deserialize_with = "deserialize_feerate")]
	pub effective_feerate: Option<FeeRate>,
	/// If [`Self::effective_feerate`] is provided, holds the
	/// [`Wtxid`]s of the transactions whose fees and virtual
	/// sizes are included in effective-feerate.
	#[serde(rename = "effective-includes")]
	pub effective_includes: Option<Vec<Wtxid>>,
}

impl EsploraTx {
	/// Convert this [`EsploraTx`] into a [`Transaction`].
	///
	/// This will drop the Esplora-specific metadata (fee, weight, confirmation status)
	/// and reconstructs the [`Transaction`] from its inputs and outputs.
	pub fn to_tx(&self) -> Transaction {
		Transaction {
			version: self.version,
			lock_time: self.locktime,
			input: self
				.vin
				.iter()
				.cloned()
				.map(|vin| TxIn {
					previous_output: OutPoint { txid: vin.txid, vout: vin.vout },
					script_sig: vin.scriptsig,
					sequence: vin.sequence,
					witness: vin.witness,
				})
				.collect(),
			output: self
				.vout
				.iter()
				.cloned()
				.map(|vout| TxOut { value: vout.value, script_pubkey: vout.scriptpubkey })
				.collect(),
		}
	}

	/// Get the confirmation time of this [`EsploraTx`].
	///
	/// If the transaction is confirmed, returns its [`BlockTime`] containing
	/// confirmation height and UNIX timestamp. If not, returns `None`.
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

	/// Get the previous [`TxOut`]s spent by this transaction's inputs.
	///
	/// Returns one [`Option<TxOut>`] per input, in order.
	/// `None` if the input spends a coinbase output.
	pub fn previous_outputs(&self) -> Vec<Option<TxOut>> {
		self.vin
			.iter()
			.cloned()
			.map(|vin| {
				vin.prevout.map(|prevout| TxOut {
					script_pubkey: prevout.scriptpubkey,
					value: prevout.value,
				})
			})
			.collect()
	}
}

impl From<EsploraTx> for Transaction {
	fn from(tx: EsploraTx) -> Self {
		tx.to_tx()
	}
}

impl From<&EsploraTx> for Transaction {
	fn from(tx: &EsploraTx) -> Self {
		tx.to_tx()
	}
}

/// Deserializes an [`Address`] from an Esplora address string.
///
/// Esplora returns address strings without separately providing the expected
/// network, so this deserializer parses the address and assumes the embedded
/// network marker is correct.
fn deserialize_address_assume_checked<'de, D>(d: D) -> Result<Address, D::Error>
where
	D: serde::de::Deserializer<'de>,
{
	let address = Address::<bitcoin::address::NetworkUnchecked>::deserialize(d)?;
	Ok(address.assume_checked())
}

/// Deserializes an optional [`FeeRate`] from an `f64` BTC/kvB value.
///
/// The Esplora API expresses effective feerates as BTC per kilovirtual-byte.
/// This deserializer converts it to sat/kwu as required by [`FeeRate`].
///
/// Returns `None` if the value is absent, and an error if the resulting
/// feerate would overflow.
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

/// Deserializes a mempool fee histogram from `(sat/vB, vsize)` entries.
///
/// The Esplora API expresses fee histogram buckets as feerates in satoshis per
/// virtual byte paired with each bucket's virtual size. This deserializer
/// converts each feerate to sat/kwu as required by [`FeeRate`].
fn deserialize_fee_histogram<'de, D>(d: D) -> Result<Vec<(FeeRate, Weight)>, D::Error>
where
	D: serde::de::Deserializer<'de>,
{
	use serde::de::Error;
	let raw = Vec::<(f64, Weight)>::deserialize(d)?;
	raw.into_iter()
		.map(|(sat_per_vb, vsize)| {
			let sat_per_kwu = sat_per_vb * 250.0;
			if !sat_per_kwu.is_finite() {
				return Err(D::Error::custom("feerate overflow"));
			}
			Ok((FeeRate::from_sat_per_kwu(sat_per_kwu as u64), vsize))
		})
		.collect()
}
