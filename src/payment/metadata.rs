use std::collections::BTreeMap;

use lightning::impl_ser_tlv_based;
use lightning::util::ser::{Readable, Writeable};
use lightning_liquidity::lsps2::router::{
	LSPS2Bolt12InvoiceParameters, LSPS2Bolt12PaymentMetadataDecoder,
};

use crate::payment::store::LSPS2Parameters;

pub(crate) const LDK_NODE_BOLT12_PAYMENT_METADATA_KEY: u64 = 0;

/// Metadata carried in BOLT11 invoice `payment_metadata` or BOLT12 payment metadata maps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentMetadata {
	pub(crate) lsps2_parameters: Option<LSPS2Parameters>,
	pub(crate) lsps2_bolt12_invoice_parameters: Option<LSPS2Bolt12InvoiceParameters>,
}

impl PaymentMetadata {
	pub(crate) fn encode_as_bolt12_payment_metadata(&self) -> BTreeMap<u64, Vec<u8>> {
		let mut metadata = BTreeMap::new();
		metadata.insert(LDK_NODE_BOLT12_PAYMENT_METADATA_KEY, self.encode());
		metadata
	}
}

impl_ser_tlv_based!(PaymentMetadata, {
	(0, lsps2_parameters, option),
	(2, lsps2_bolt12_invoice_parameters, option),
});

#[derive(Clone, Copy)]
pub(crate) struct LdkNodeLSPS2Bolt12PaymentMetadataDecoder;

impl LSPS2Bolt12PaymentMetadataDecoder for LdkNodeLSPS2Bolt12PaymentMetadataDecoder {
	fn decode_lsps2_invoice_parameters(
		&self, payment_metadata: &BTreeMap<u64, Vec<u8>>,
	) -> Vec<LSPS2Bolt12InvoiceParameters> {
		payment_metadata
			.get(&LDK_NODE_BOLT12_PAYMENT_METADATA_KEY)
			.and_then(|encoded| PaymentMetadata::read(&mut &encoded[..]).ok())
			.and_then(|metadata| metadata.lsps2_bolt12_invoice_parameters)
			.into_iter()
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
	use lightning::util::ser::{Readable, Writeable};

	use super::*;

	fn pubkey(byte: u8) -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
	}

	#[test]
	fn empty_metadata_roundtrips() {
		let metadata =
			PaymentMetadata { lsps2_parameters: None, lsps2_bolt12_invoice_parameters: None };

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
		let lsps2_bolt12_invoice_parameters = LSPS2Bolt12InvoiceParameters {
			counterparty_node_id: pubkey(2),
			intercept_scid: 42,
			cltv_expiry_delta: 144,
		};
		let metadata = PaymentMetadata {
			lsps2_parameters: Some(lsps2_parameters),
			lsps2_bolt12_invoice_parameters: Some(lsps2_bolt12_invoice_parameters),
		};

		let encoded = metadata.encode();
		let decoded = PaymentMetadata::read(&mut &*encoded).unwrap();

		assert_eq!(metadata, decoded);
	}

	#[test]
	fn bolt12_metadata_decoder_extracts_invoice_parameters() {
		let lsps2_bolt12_invoice_parameters = LSPS2Bolt12InvoiceParameters {
			counterparty_node_id: pubkey(3),
			intercept_scid: 43,
			cltv_expiry_delta: 72,
		};
		let metadata = PaymentMetadata {
			lsps2_parameters: None,
			lsps2_bolt12_invoice_parameters: Some(lsps2_bolt12_invoice_parameters),
		}
		.encode_as_bolt12_payment_metadata();

		let decoded =
			LdkNodeLSPS2Bolt12PaymentMetadataDecoder.decode_lsps2_invoice_parameters(&metadata);

		assert_eq!(decoded, vec![lsps2_bolt12_invoice_parameters]);
	}
}
