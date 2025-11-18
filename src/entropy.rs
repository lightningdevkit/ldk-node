// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Contains utilities for configuring and generating entropy.

use bip39::Mnemonic;

/// Generates a random [BIP 39] mnemonic with the specified word count.
///
/// If no word count is specified, defaults to 24 words (256-bit entropy).
///
/// The result may be used to initialize the [`Node`] entropy, i.e., can be given to
/// [`Builder::set_entropy_bip39_mnemonic`].
///
/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
/// [`Node`]: crate::Node
/// [`Builder::set_entropy_bip39_mnemonic`]: crate::Builder::set_entropy_bip39_mnemonic
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
