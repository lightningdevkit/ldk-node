// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Contains utilities for configuring and generating entropy.

use std::fmt;

use bip39::Mnemonic;

use crate::config::WALLET_KEYS_SEED_LEN;
use crate::io;

/// An error that could arise during [`NodeEntropy`] construction.
#[derive(Debug, Clone, PartialEq)]
pub enum EntropyError {
	/// The given seed bytes are invalid, e.g., have invalid length.
	InvalidSeedBytes,
	/// The given seed file is invalid, e.g., has invalid length, or could not be read.
	InvalidSeedFile,
}

impl fmt::Display for EntropyError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::InvalidSeedBytes => write!(f, "Given seed bytes are invalid."),
			Self::InvalidSeedFile => write!(f, "Given seed file is invalid or could not be read."),
		}
	}
}

impl std::error::Error for EntropyError {}

/// The node entropy, i.e., the main secret from which all other secrets of the [`Node`] are
/// derived.
///
/// [`Node`]: crate::Node
#[derive(Copy, Clone)]
pub struct NodeEntropy([u8; WALLET_KEYS_SEED_LEN]);

impl NodeEntropy {
	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	/// [`Node`]: crate::Node
	pub fn from_bip39_mnemonic(mnemonic: Mnemonic, passphrase: Option<String>) -> Self {
		match passphrase {
			Some(passphrase) => Self(mnemonic.to_seed(passphrase)),
			None => Self(mnemonic.to_seed("")),
		}
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	///
	/// [`Node`]: crate::Node
	#[cfg(not(feature = "uniffi"))]
	pub fn from_seed_bytes(seed_bytes: [u8; WALLET_KEYS_SEED_LEN]) -> Self {
		Self(seed_bytes)
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	///
	/// Will return an error if the length of the given `Vec` is not exactly
	/// [`WALLET_KEYS_SEED_LEN`].
	///
	/// [`Node`]: crate::Node
	#[cfg(feature = "uniffi")]
	pub fn from_seed_bytes(seed_bytes: Vec<u8>) -> Result<NodeEntropy, EntropyError> {
		if seed_bytes.len() != WALLET_KEYS_SEED_LEN {
			return Err(EntropyError::InvalidSeedBytes);
		}
		let mut seed_bytes_inner = [0u8; WALLET_KEYS_SEED_LEN];
		seed_bytes_inner.copy_from_slice(&seed_bytes);
		Ok(Self(seed_bytes_inner))
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	///
	/// [`Node`]: crate::Node
	pub fn from_seed_path(seed_path: String) -> Result<Self, EntropyError> {
		Ok(Self(
			io::utils::read_or_generate_seed_file(&seed_path)
				.map_err(|_| EntropyError::InvalidSeedFile)?,
		))
	}

	pub(crate) fn to_seed_bytes(&self) -> [u8; WALLET_KEYS_SEED_LEN] {
		self.0
	}
}

impl fmt::Display for NodeEntropy {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "NODE ENTROPY")
	}
}

impl fmt::Debug for NodeEntropy {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "NODE ENTROPY")
	}
}

/// Generates a random [BIP 39] mnemonic with the specified word count.
///
/// If no word count is specified, defaults to 24 words (256-bit entropy).
///
/// The result may be used to initialize the [`NodeEntropy`], i.e., can be given to
/// [`NodeEntropy::from_bip39_mnemonic`].
///
/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
/// [`Node`]: crate::Node
pub fn generate_entropy_mnemonic(word_count: Option<WordCount>) -> Mnemonic {
	let word_count = word_count.unwrap_or(WordCount::Words24).word_count();
	Mnemonic::generate(word_count).expect("Failed to generate mnemonic")
}

/// Supported BIP39 mnemonic word counts for entropy generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCount {
	/// 12-word mnemonic (128-bit entropy)
	Words12,
	/// 15-word mnemonic (160-bit entropy)
	Words15,
	/// 18-word mnemonic (192-bit entropy)
	Words18,
	/// 21-word mnemonic (224-bit entropy)
	Words21,
	/// 24-word mnemonic (256-bit entropy)
	Words24,
}

impl WordCount {
	/// Returns the word count as a usize value.
	pub fn word_count(&self) -> usize {
		match self {
			WordCount::Words12 => 12,
			WordCount::Words15 => 15,
			WordCount::Words18 => 18,
			WordCount::Words21 => 21,
			WordCount::Words24 => 24,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mnemonic_to_entropy_to_mnemonic() {
		// Test default (24 words)
		let mnemonic = generate_entropy_mnemonic(None);
		let entropy = mnemonic.to_entropy();
		assert_eq!(mnemonic, Mnemonic::from_entropy(&entropy).unwrap());
		assert_eq!(mnemonic.word_count(), 24);

		// Test with different word counts
		let word_counts = [
			WordCount::Words12,
			WordCount::Words15,
			WordCount::Words18,
			WordCount::Words21,
			WordCount::Words24,
		];

		for word_count in word_counts {
			let mnemonic = generate_entropy_mnemonic(Some(word_count));
			let entropy = mnemonic.to_entropy();
			assert_eq!(mnemonic, Mnemonic::from_entropy(&entropy).unwrap());

			// Verify expected word count
			let expected_words = match word_count {
				WordCount::Words12 => 12,
				WordCount::Words15 => 15,
				WordCount::Words18 => 18,
				WordCount::Words21 => 21,
				WordCount::Words24 => 24,
			};
			assert_eq!(mnemonic.word_count(), expected_words);
		}
	}
}
