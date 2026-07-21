// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>.

//! Router helpers for injecting LSPS2 blinded payment paths.

use bitcoin::secp256k1::{self, PublicKey, Secp256k1};

use lightning::blinded_path::payment::{
	BlindedPaymentPath, ForwardTlvs, PaymentConstraints, PaymentContext, PaymentForwardNode,
	PaymentRelay, ReceiveTlvs,
};
use lightning::impl_writeable_tlv_based;
use lightning::ln::channel_state::ChannelDetails;
use lightning::ln::channelmanager::{PaymentId, MIN_FINAL_CLTV_EXPIRY_DELTA};
use lightning::routing::router::{InFlightHtlcs, Route, RouteParameters, Router};
use lightning::sign::{EntropySource, ReceiveAuthKey};
use lightning::types::features::BlindedHopFeatures;
use lightning::types::payment::PaymentHash;

use crate::payment::PaymentMetadata;

/// Parameters needed to construct an LSPS2 blinded payment path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LSPS2LeaseParameters {
	/// The LSP serving as the blinded path introduction node.
	pub lsp_node_id: PublicKey,
	/// The single-use intercept short channel id negotiated with the LSP.
	pub intercept_scid: u64,
	/// The CLTV delta required by the LSP.
	pub cltv_expiry_delta: u16,
	/// The fixed amount negotiated for this intercept SCID, or `None` for a variable amount.
	pub payment_size_msat: Option<u64>,
	/// Unix timestamp at which these parameters expire.
	pub valid_until: u64,
}

impl_writeable_tlv_based!(LSPS2LeaseParameters, {
	(0, lsp_node_id, required),
	(2, intercept_scid, required),
	(4, cltv_expiry_delta, required),
	(6, payment_size_msat, option),
	(8, valid_until, required),
});

/// A router wrapper that uses ordinary payment paths when available and LSPS2 JIT paths otherwise.
pub struct LSPS2Router<R: Router, ES: EntropySource> {
	inner_router: R,
	entropy_source: ES,
}

impl<R: Router, ES: EntropySource> LSPS2Router<R, ES> {
	/// Constructs an LSPS2-aware wrapper around `inner_router`.
	pub fn new(inner_router: R, entropy_source: ES) -> Self {
		Self { inner_router, entropy_source }
	}

	fn payment_parameters(&self, payment_context: &PaymentContext) -> Vec<LSPS2LeaseParameters> {
		match payment_context {
			PaymentContext::Bolt12Offer(_) | PaymentContext::AsyncBolt12Offer(_) => {},
			_ => return Vec::new(),
		}
		payment_context
			.payment_metadata()
			.and_then(PaymentMetadata::decode_from_bolt12_payment_metadata)
			.and_then(|metadata| metadata.lsps2_lease_parameters)
			.into_iter()
			.collect()
	}
}

impl<R: Router, ES: EntropySource> Router for LSPS2Router<R, ES> {
	fn find_route(
		&self, payer: &PublicKey, route_params: &RouteParameters,
		first_hops: Option<&[&ChannelDetails]>, inflight_htlcs: InFlightHtlcs,
	) -> Result<Route, &'static str> {
		self.inner_router.find_route(payer, route_params, first_hops, inflight_htlcs)
	}

	fn find_route_with_id(
		&self, payer: &PublicKey, route_params: &RouteParameters,
		first_hops: Option<&[&ChannelDetails]>, inflight_htlcs: InFlightHtlcs,
		payment_hash: PaymentHash, payment_id: PaymentId,
	) -> Result<Route, &'static str> {
		self.inner_router.find_route_with_id(
			payer,
			route_params,
			first_hops,
			inflight_htlcs,
			payment_hash,
			payment_id,
		)
	}

	fn create_blinded_payment_paths<T: secp256k1::Signing + secp256k1::Verification>(
		&self, recipient: PublicKey, local_node_receive_key: ReceiveAuthKey,
		first_hops: Vec<ChannelDetails>, tlvs: ReceiveTlvs, amount_msats: Option<u64>,
		secp_ctx: &Secp256k1<T>,
	) -> Result<Vec<BlindedPaymentPath>, ()> {
		let inner_paths = self.inner_router.create_blinded_payment_paths(
			recipient,
			local_node_receive_key,
			first_hops,
			tlvs.clone(),
			amount_msats,
			secp_ctx,
		);
		// The inner router was given the resolved payment amount and only returns ordinary paths when
		// the existing inbound liquidity can receive all of it. Always prefer those paths. Besides
		// avoiding an unnecessary channel open, this prevents an MPP payer from splitting one payment
		// across regular and JIT paths. The LSP only opens its channel after receiving the complete
		// negotiated amount on the intercept SCID, so a partial JIT shard could otherwise remain stuck
		// indefinitely.
		if matches!(&inner_paths, Ok(paths) if !paths.is_empty()) {
			return inner_paths;
		}

		let parameters = self.payment_parameters(&tlvs.payment_context);
		if parameters.is_empty() {
			return inner_paths;
		}
		let Some(amount_msats) = amount_msats else {
			// Invoice construction supplies the resolved amount even for a variable-amount offer. Without
			// it, we cannot constrain the JIT path to the full channel-open trigger amount.
			return inner_paths;
		};

		let mut paths = Vec::new();
		for params in parameters {
			// A fixed lease is valid only for the exact amount negotiated with the LSP. A mismatch means
			// these parameters were selected for a different response and must never be exposed.
			if params.payment_size_msat.is_some_and(|fixed_amount| fixed_amount != amount_msats) {
				continue;
			}
			// Both fixed and variable invoices require the entire resolved payment on any selected JIT
			// path. Fixed invoices may advertise MPP across multiple regular paths, while variable
			// invoices disable MPP at response construction. Setting both bounds here additionally makes
			// every JIT candidate indivisible and lets us include several candidates for robustness.
			let htlc_amount_msat = amount_msats;
			let payment_constraints = PaymentConstraints {
				max_cltv_expiry: tlvs
					.payment_constraints
					.max_cltv_expiry
					.saturating_add(params.cltv_expiry_delta as u32),
				htlc_minimum_msat: htlc_amount_msat,
			};
			let forward_node = PaymentForwardNode {
				tlvs: ForwardTlvs {
					short_channel_id: params.intercept_scid,
					payment_relay: PaymentRelay {
						cltv_expiry_delta: params.cltv_expiry_delta,
						fee_proportional_millionths: 0,
						fee_base_msat: 0,
					},
					payment_constraints,
					features: BlindedHopFeatures::empty(),
					next_blinding_override: None,
				},
				node_id: params.lsp_node_id,
				htlc_maximum_msat: htlc_amount_msat,
			};
			if let Ok(path) = BlindedPaymentPath::new(
				&[forward_node],
				recipient,
				local_node_receive_key,
				tlvs.clone(),
				htlc_amount_msat,
				MIN_FINAL_CLTV_EXPIRY_DELTA,
				&self.entropy_source,
				secp_ctx,
			) {
				paths.push(path);
			}
		}
		if paths.is_empty() {
			// A capacity race may make ordinary paths available after a lease was negotiated. That case
			// returned above and intentionally discards the single-use lease. Reaching here means neither
			// ordinary nor valid JIT paths can receive the payment.
			Err(())
		} else {
			Ok(paths)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use bitcoin::secp256k1::SecretKey;
	use core::sync::atomic::{AtomicUsize, Ordering};
	use lightning::blinded_path::payment::{Bolt12OfferContext, PaymentConstraints};
	use lightning::offers::invoice_request::InvoiceRequestFields;
	use lightning::offers::offer::OfferId;
	use lightning::types::payment::PaymentSecret;
	use std::collections::BTreeMap;

	#[derive(Clone)]
	struct TestEntropy;

	impl EntropySource for TestEntropy {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			[42; 32]
		}
	}

	struct MockRouter {
		calls: AtomicUsize,
		return_regular_path: bool,
	}

	impl Router for MockRouter {
		fn find_route(
			&self, _payer: &PublicKey, _route_params: &RouteParameters,
			_first_hops: Option<&[&ChannelDetails]>, _inflight_htlcs: InFlightHtlcs,
		) -> Result<Route, &'static str> {
			Err("mock router")
		}

		fn create_blinded_payment_paths<T: secp256k1::Signing + secp256k1::Verification>(
			&self, recipient: PublicKey, local_node_receive_key: ReceiveAuthKey,
			_first_hops: Vec<ChannelDetails>, tlvs: ReceiveTlvs, _amount_msats: Option<u64>,
			secp_ctx: &Secp256k1<T>,
		) -> Result<Vec<BlindedPaymentPath>, ()> {
			self.calls.fetch_add(1, Ordering::AcqRel);
			if self.return_regular_path {
				BlindedPaymentPath::one_hop(
					recipient,
					local_node_receive_key,
					tlvs,
					MIN_FINAL_CLTV_EXPIRY_DELTA,
					TestEntropy,
					secp_ctx,
				)
				.map(|path| vec![path])
			} else {
				Err(())
			}
		}
	}

	fn pubkey(byte: u8) -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[byte; 32]).unwrap())
	}

	fn payment_tlvs(metadata: BTreeMap<u64, Vec<u8>>) -> ReceiveTlvs {
		ReceiveTlvs {
			payment_secret: PaymentSecret([2; 32]),
			payment_constraints: PaymentConstraints { max_cltv_expiry: 100, htlc_minimum_msat: 1 },
			payment_context: PaymentContext::Bolt12Offer(Bolt12OfferContext {
				offer_id: OfferId([8; 32]),
				invoice_request: InvoiceRequestFields {
					payer_signing_pubkey: pubkey(9),
					quantity: None,
					payer_note_truncated: None,
					human_readable_name: None,
				},
				payment_metadata: Some(metadata),
			}),
		}
	}

	fn payment_metadata(parameters: LSPS2LeaseParameters) -> BTreeMap<u64, Vec<u8>> {
		PaymentMetadata { lsps2_parameters: None, lsps2_lease_parameters: Some(parameters) }
			.encode_as_bolt12_payment_metadata()
	}

	#[test]
	fn constrains_fixed_jit_path_to_negotiated_amount() {
		let lsp_node_id = pubkey(11);
		let parameters = LSPS2LeaseParameters {
			lsp_node_id,
			intercept_scid: 42,
			cltv_expiry_delta: 48,
			payment_size_msat: Some(3_000),
			valid_until: u64::MAX,
		};
		let metadata = payment_metadata(parameters);
		let inner_router = MockRouter { calls: AtomicUsize::new(0), return_regular_path: false };
		let router = LSPS2Router::new(inner_router, TestEntropy);

		let paths = router
			.create_blinded_payment_paths(
				pubkey(10),
				ReceiveAuthKey([3; 32]),
				Vec::new(),
				payment_tlvs(metadata),
				Some(3_000),
				&Secp256k1::new(),
			)
			.unwrap();

		assert_eq!(router.inner_router.calls.load(Ordering::Acquire), 1);
		assert_eq!(paths.len(), 1);
		assert_eq!(
			paths[0].introduction_node(),
			&lightning::blinded_path::IntroductionNode::NodeId(lsp_node_id)
		);
		assert_eq!(paths[0].payinfo.htlc_minimum_msat, 3_000);
		assert_eq!(paths[0].payinfo.htlc_maximum_msat, 3_000);
	}

	#[test]
	fn rejects_fixed_jit_path_for_another_amount() {
		let parameters = LSPS2LeaseParameters {
			lsp_node_id: pubkey(11),
			intercept_scid: 42,
			cltv_expiry_delta: 48,
			payment_size_msat: Some(3_000),
			valid_until: u64::MAX,
		};
		let metadata = payment_metadata(parameters);
		let inner_router = MockRouter { calls: AtomicUsize::new(0), return_regular_path: false };
		let router = LSPS2Router::new(inner_router, TestEntropy);

		assert!(router
			.create_blinded_payment_paths(
				pubkey(10),
				ReceiveAuthKey([3; 32]),
				Vec::new(),
				payment_tlvs(metadata),
				Some(5_000),
				&Secp256k1::new(),
			)
			.is_err());
	}

	#[test]
	fn constrains_variable_jit_path_to_invoice_amount() {
		let parameters = LSPS2LeaseParameters {
			lsp_node_id: pubkey(11),
			intercept_scid: 42,
			cltv_expiry_delta: 48,
			payment_size_msat: None,
			valid_until: u64::MAX,
		};
		let metadata = payment_metadata(parameters);
		let inner_router = MockRouter { calls: AtomicUsize::new(0), return_regular_path: false };
		let router = LSPS2Router::new(inner_router, TestEntropy);

		let paths = router
			.create_blinded_payment_paths(
				pubkey(10),
				ReceiveAuthKey([3; 32]),
				Vec::new(),
				payment_tlvs(metadata),
				Some(5_000),
				&Secp256k1::new(),
			)
			.unwrap();

		assert_eq!(paths[0].payinfo.htlc_minimum_msat, 5_000);
		assert_eq!(paths[0].payinfo.htlc_maximum_msat, 5_000);
	}

	#[test]
	fn omits_jit_path_when_regular_paths_are_available() {
		let lsp_node_id = pubkey(11);
		let parameters = LSPS2LeaseParameters {
			lsp_node_id,
			intercept_scid: 42,
			cltv_expiry_delta: 48,
			payment_size_msat: Some(3_000),
			valid_until: u64::MAX,
		};
		let metadata = payment_metadata(parameters);
		let inner_router = MockRouter { calls: AtomicUsize::new(0), return_regular_path: true };
		let router = LSPS2Router::new(inner_router, TestEntropy);
		let recipient = pubkey(10);

		let paths = router
			.create_blinded_payment_paths(
				recipient,
				ReceiveAuthKey([3; 32]),
				Vec::new(),
				payment_tlvs(metadata),
				Some(3_000),
				&Secp256k1::new(),
			)
			.unwrap();

		assert_eq!(router.inner_router.calls.load(Ordering::Acquire), 1);
		assert_eq!(paths.len(), 1);
		assert_ne!(
			paths[0].introduction_node(),
			&lightning::blinded_path::IntroductionNode::NodeId(lsp_node_id)
		);
	}
}
