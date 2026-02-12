// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Core types for the aggregate wallet library.

use std::fmt;

use bitcoin::{psbt, OutPoint, Weight};

/// Errors returned by the aggregate wallet library.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
	/// A wallet for the requested key was not found.
	WalletNotFound,
	/// A persister for the requested key was not found.
	PersisterNotFound,
	/// Failed to persist wallet state.
	PersistenceFailed,
	/// On-chain transaction creation failed.
	OnchainTxCreationFailed,
	/// On-chain transaction signing failed.
	OnchainTxSigningFailed,
	/// Insufficient funds for the requested operation.
	InsufficientFunds,
	/// The requested fee rate is invalid.
	InvalidFeeRate,
	/// No spendable outputs available.
	NoSpendableOutputs,
	/// Coin selection failed.
	CoinSelectionFailed,
	/// A generic wallet operation failed.
	WalletOperationFailed,
	/// UTXO not found locally (caller may want to try a chain source fallback).
	UtxoNotFoundLocally(OutPoint),
	/// Transaction not found in any wallet.
	TransactionNotFound,
	/// Cannot replace a transaction that is already confirmed.
	TransactionAlreadyConfirmed,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Error::WalletNotFound => write!(f, "Wallet not found"),
			Error::PersisterNotFound => write!(f, "Persister not found"),
			Error::PersistenceFailed => write!(f, "Failed to persist wallet state"),
			Error::OnchainTxCreationFailed => write!(f, "On-chain transaction creation failed"),
			Error::OnchainTxSigningFailed => write!(f, "On-chain transaction signing failed"),
			Error::InsufficientFunds => write!(f, "Insufficient funds"),
			Error::InvalidFeeRate => write!(f, "Invalid fee rate"),
			Error::NoSpendableOutputs => write!(f, "No spendable outputs available"),
			Error::CoinSelectionFailed => write!(f, "Coin selection failed"),
			Error::WalletOperationFailed => write!(f, "Wallet operation failed"),
			Error::UtxoNotFoundLocally(op) => {
				write!(f, "UTXO {:?} not found in any local wallet", op)
			},
			Error::TransactionNotFound => write!(f, "Transaction not found in any wallet"),
			Error::TransactionAlreadyConfirmed => {
				write!(f, "Cannot replace an already-confirmed transaction")
			},
		}
	}
}

impl std::error::Error for Error {}

/// UTXO info for multi-wallet PSBT building.
#[derive(Clone)]
pub struct UtxoPsbtInfo {
	/// The outpoint being spent.
	pub outpoint: OutPoint,
	/// The PSBT input data for this UTXO.
	pub psbt_input: psbt::Input,
	/// The satisfaction weight for this input type.
	pub weight: Weight,
	/// Whether this UTXO belongs to the primary wallet.
	pub is_primary: bool,
}

/// Available coin selection algorithms.
#[derive(Debug, Clone, Copy)]
pub enum CoinSelectionAlgorithm {
	/// Branch and bound algorithm (tries to find exact match).
	BranchAndBound,
	/// Select largest UTXOs first.
	LargestFirst,
	/// Select oldest UTXOs first.
	OldestFirst,
	/// Select UTXOs randomly.
	SingleRandomDraw,
}
