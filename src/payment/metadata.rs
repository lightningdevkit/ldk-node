// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::BTreeMap;

use lightning::impl_writeable_tlv_based;
use lightning::util::ser::{Readable, Writeable};

use crate::liquidity::client::lsps2::router::LSPS2LeaseParameters;
use crate::payment::store::LSPS2Parameters;

pub(crate) const LDK_NODE_BOLT12_PAYMENT_METADATA_KEY: u64 = 0;

/// Metadata carried in BOLT11 invoice metadata or BOLT12 payment-context metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadata {
	/// Fee limits retained until payment claim to validate the LSP's withholding.
	pub(crate) lsps2_parameters: Option<LSPS2Parameters>,
	/// Single-use routing parameters consumed while constructing blinded payment paths.
	pub(crate) lsps2_lease_parameters: Option<LSPS2LeaseParameters>,
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

#[cfg(test)]
mod tests {
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

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
