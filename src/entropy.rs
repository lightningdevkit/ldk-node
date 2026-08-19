// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Contains utilities for configuring and generating entropy.

use std::fmt;

use crate::config::WALLET_KEYS_SEED_LEN;
use crate::ffi::maybe_deref;
use crate::io;

#[cfg(not(feature = "uniffi"))]
type Mnemonic = bip39::Mnemonic;
#[cfg(feature = "uniffi")]
type Mnemonic = std::sync::Arc<crate::ffi::Mnemonic>;

/// An error that could arise during [`NodeEntropy`] construction.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct NodeEntropy([u8; WALLET_KEYS_SEED_LEN]);

impl NodeEntropy {
	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	///
	/// [`Node`]: crate::Node
	#[cfg(not(feature = "uniffi"))]
	pub fn from_seed_bytes(seed_bytes: [u8; WALLET_KEYS_SEED_LEN]) -> Self {
		Self(seed_bytes)
	}

	pub(crate) fn to_seed_bytes(&self) -> [u8; WALLET_KEYS_SEED_LEN] {
		self.0
	}
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl NodeEntropy {
	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	/// [`Node`]: crate::Node
	#[cfg_attr(feature = "uniffi", uniffi::constructor)]
	pub fn from_bip39_mnemonic(mnemonic: Mnemonic, passphrase: Option<String>) -> Self {
		let mnemonic = maybe_deref(&mnemonic);
		match passphrase {
			Some(passphrase) => Self(mnemonic.to_seed(passphrase)),
			None => Self(mnemonic.to_seed("")),
		}
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given
	/// [`WALLET_KEYS_SEED_LEN`] seed bytes.
	///
	/// Will return an error if the length of the given `Vec` is not exactly
	/// [`WALLET_KEYS_SEED_LEN`].
	///
	/// [`Node`]: crate::Node
	#[cfg(feature = "uniffi")]
	#[uniffi::constructor]
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
	#[cfg_attr(feature = "uniffi", uniffi::constructor)]
	pub fn from_seed_path(seed_path: String) -> Result<Self, EntropyError> {
		Ok(Self(
			io::utils::read_or_generate_seed_file(&seed_path)
				.map_err(|_| EntropyError::InvalidSeedFile)?,
		))
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
