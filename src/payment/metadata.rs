// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::BTreeMap;

use lightning::impl_writeable_tlv_based;
use lightning::util::ser::{Readable, Writeable};
use lightning_liquidity::lsps2::router::{PaymentMetadataDecoder, PaymentParameters};

use crate::payment::store::LSPS2Parameters;

pub(crate) const LDK_NODE_BOLT12_PAYMENT_METADATA_KEY: u64 = 0;

/// Metadata carried in BOLT11 invoice metadata or BOLT12 payment-context metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadata {
	/// Fee limits retained until payment claim to validate the LSP's withholding.
	pub(crate) lsps2_parameters: Option<LSPS2Parameters>,
	/// Single-use routing parameters consumed while constructing blinded payment paths.
	pub(crate) lsps2_lease_parameters: Option<PaymentParameters>,
}

impl PaymentMetadata {
	pub(crate) fn encode_as_bolt12_payment_metadata(&self) -> BTreeMap<u64, Vec<u8>> {
		let mut metadata = BTreeMap::new();
		metadata.insert(LDK_NODE_BOLT12_PAYMENT_METADATA_KEY, self.encode());
		metadata
	}

	pub(crate) fn decode_from_bolt12_payment_metadata(
		payment_metadata: &BTreeMap<u64, Vec<u8>>,
	) -> Option<Self> {
		payment_metadata
			.get(&LDK_NODE_BOLT12_PAYMENT_METADATA_KEY)
			.and_then(|encoded| Self::read(&mut &encoded[..]).ok())
	}
}

impl_writeable_tlv_based!(PaymentMetadata, {
	(0, lsps2_parameters, option),
	(2, lsps2_lease_parameters, option),
});

#[derive(Clone, Copy)]
pub(crate) struct LSPS2PaymentMetadataDecoder;

impl PaymentMetadataDecoder for LSPS2PaymentMetadataDecoder {
	fn decode_payment_parameters(
		&self, payment_metadata: &BTreeMap<u64, Vec<u8>>,
	) -> Vec<PaymentParameters> {
		PaymentMetadata::decode_from_bolt12_payment_metadata(payment_metadata)
			.and_then(|metadata| metadata.lsps2_lease_parameters)
			.into_iter()
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
	use lightning::util::ser::{Readable, Writeable};
	use lightning_liquidity::lsps2::router::{PaymentMetadataDecoder, PaymentParameters};

	use super::*;

	fn pubkey(byte: u8) -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
	}

	#[test]
	fn empty_metadata_roundtrips() {
		let metadata = PaymentMetadata { lsps2_parameters: None, lsps2_lease_parameters: None };

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}

	#[test]
	fn lsps2_parameters_roundtrip() {
		let lsps2_parameters = LSPS2Parameters {
			max_total_opening_fee_msat: Some(42_000),
			max_proportional_opening_fee_ppm_msat: Some(17_000),
		};
		let metadata = PaymentMetadata {
			lsps2_parameters: Some(lsps2_parameters),
			lsps2_lease_parameters: None,
		};

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}

	#[test]
	fn bolt12_decoder_extracts_lease_parameters() {
		let lease_parameters = PaymentParameters {
			lsp_node_id: pubkey(3),
			intercept_scid: 43,
			cltv_expiry_delta: 72,
			payment_size_msat: None,
			valid_until: 1_000_000,
		};
		let metadata = PaymentMetadata {
			lsps2_parameters: None,
			lsps2_lease_parameters: Some(lease_parameters),
		}
		.encode_as_bolt12_payment_metadata();

		assert_eq!(
			LSPS2PaymentMetadataDecoder.decode_payment_parameters(&metadata),
			vec![lease_parameters]
		);
	}

	#[test]
	fn bolt12_metadata_roundtrips() {
		let metadata = PaymentMetadata {
			lsps2_parameters: Some(LSPS2Parameters {
				max_total_opening_fee_msat: Some(42_000),
				max_proportional_opening_fee_ppm_msat: None,
			}),
			lsps2_lease_parameters: None,
		};

		let encoded = metadata.encode_as_bolt12_payment_metadata();

		assert_eq!(PaymentMetadata::decode_from_bolt12_payment_metadata(&encoded), Some(metadata));
	}
}
