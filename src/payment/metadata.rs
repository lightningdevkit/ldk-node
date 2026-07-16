// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use lightning::impl_writeable_tlv_based;

use crate::payment::store::LSPS2Parameters;

/// Metadata carried in BOLT11 invoice `payment_metadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadata {
	pub(crate) lsps2_parameters: Option<LSPS2Parameters>,
}

impl_writeable_tlv_based!(PaymentMetadata, {
	(0, lsps2_parameters, option),
});

#[cfg(test)]
mod tests {
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	#[test]
	fn empty_metadata_roundtrips() {
		let metadata = PaymentMetadata { lsps2_parameters: None };

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
		let metadata = PaymentMetadata { lsps2_parameters: Some(lsps2_parameters) };

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}
}
