// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock, Weak};

use bitcoin::block::Header;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::{Block, BlockHash, Network, Txid};
use lightning::blinded_path::message::{MessageContext, OffersContext};
use lightning::chain::transaction::TransactionData;
use lightning::chain::{BlockLocator, Confirm, Listen};
use lightning::offers::flow::{InvreqResponseInstructions, OffersMessageFlow};
use lightning::offers::invoice::{Bolt12Invoice, UnsignedBolt12Invoice};
use lightning::offers::invoice_error::InvoiceError;
use lightning::offers::invoice_request::InvoiceRequestVerifiedFromOffer;
use lightning::offers::offer::Amount;
use lightning::offers::parse::Bolt12SemanticError;
use lightning::onion_message::messenger::{
	MessageSendInstructions, Responder, ResponseInstruction,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};
use lightning::sign::{EntropySource, NodeSigner, Recipient};
use lightning::types::payment::{PaymentHash, PaymentSecret};
use lightning::util::logger::Logger as _;

use crate::connection::ConnectionManager;
use crate::liquidity::client::lsps2::{JitInvoiceRequest, JitInvoiceResponse, LSPS2Client};
use crate::logger::{log_error, Logger};
use crate::runtime::Runtime;
use crate::types::{ChannelManager, KeysManager, MessageRouter, OnionMessenger, Router};

type NodeOffersFlow = OffersMessageFlow<Arc<MessageRouter>, Arc<Logger>>;
type InvoicePaymentInfo = OnceLock<(u64, PaymentHash, PaymentSecret)>;

enum InvoiceBuildError {
	Semantic(Bolt12SemanticError),
	Response(InvoiceError),
}

impl InvoiceBuildError {
	fn into_invoice_error(self) -> InvoiceError {
		match self {
			Self::Semantic(error) => InvoiceError::from(error),
			Self::Response(error) => error,
		}
	}
}

struct JitInvoiceRequestDependencies {
	runtime: Arc<Runtime>,
	lsps2_client: Arc<LSPS2Client<Arc<Logger>>>,
	connection_manager: Weak<ConnectionManager<Arc<Logger>>>,
	onion_messenger: Weak<OnionMessenger>,
}

/// Routes offers messages through node-local handling before falling back to the channel manager.
pub(crate) struct NodeOffersMessageHandler {
	channel_manager: Arc<ChannelManager>,
	keys_manager: Arc<KeysManager>,
	router: Arc<Router>,
	flow: Arc<NodeOffersFlow>,
	secp_ctx: Arc<Secp256k1<bitcoin::secp256k1::All>>,
	best_block: RwLock<BlockLocator>,
	jit_dependencies: OnceLock<JitInvoiceRequestDependencies>,
	logger: Arc<Logger>,
}

impl NodeOffersMessageHandler {
	pub(crate) fn new(
		network: Network, current_timestamp: u32, channel_manager: Arc<ChannelManager>,
		keys_manager: Arc<KeysManager>, router: Arc<Router>, message_router: Arc<MessageRouter>,
		logger: Arc<Logger>,
	) -> Self {
		let best_block = channel_manager.current_best_block();
		let mut secp_ctx = Secp256k1::new();
		secp_ctx.seeded_randomize(&keys_manager.get_secure_random_bytes());
		let flow = Arc::new(OffersMessageFlow::new(
			ChainHash::using_genesis_block(network),
			best_block,
			keys_manager.get_node_id(Recipient::Node).expect("node ID"),
			current_timestamp,
			keys_manager.get_expanded_key(),
			keys_manager.get_receive_auth_key(),
			secp_ctx.clone(),
			message_router,
			Arc::clone(&logger),
		));
		Self {
			channel_manager,
			keys_manager,
			router,
			flow,
			secp_ctx: Arc::new(secp_ctx),
			best_block: RwLock::new(best_block),
			jit_dependencies: OnceLock::new(),
			logger,
		}
	}

	pub(crate) fn initialize_jit_handling(
		&self, runtime: Arc<Runtime>, lsps2_client: Arc<LSPS2Client<Arc<Logger>>>,
		connection_manager: Weak<ConnectionManager<Arc<Logger>>>,
		onion_messenger: Weak<OnionMessenger>,
	) {
		let dependencies = JitInvoiceRequestDependencies {
			runtime,
			lsps2_client,
			connection_manager,
			onion_messenger,
		};
		assert!(
			self.jit_dependencies.set(dependencies).is_ok(),
			"JIT invoice handling must only be initialized once"
		);
	}

	pub(crate) fn current_best_block(&self) -> BlockLocator {
		*self.best_block.read().expect("lock")
	}
}

fn build_invoice(
	flow: &NodeOffersFlow, channel_manager: &ChannelManager, keys_manager: &KeysManager,
	router: &Router, secp_ctx: &Secp256k1<bitcoin::secp256k1::All>,
	invoice_request: &InvoiceRequestVerifiedFromOffer,
	payment_metadata: Option<BTreeMap<u64, Vec<u8>>>, allow_mpp: bool,
	payment_info: &InvoicePaymentInfo,
) -> Result<(Bolt12Invoice, MessageContext), InvoiceBuildError> {
	let get_payment_info = |amount_msats, relative_expiry| {
		if let Some((cached_amount_msats, payment_hash, payment_secret)) = payment_info.get() {
			return (*cached_amount_msats == amount_msats)
				.then_some((*payment_hash, *payment_secret))
				.ok_or(Bolt12SemanticError::InvalidAmount);
		}
		let (payment_hash, payment_secret, _) = channel_manager
			.create_inbound_payment(Some(amount_msats), relative_expiry, None, None)
			.map_err(|_| Bolt12SemanticError::InvalidAmount)?;
		let _ = payment_info.set((amount_msats, payment_hash, payment_secret));
		Ok((payment_hash, payment_secret))
	};

	match invoice_request {
		InvoiceRequestVerifiedFromOffer::DerivedKeys(request) => {
			let (builder, context) = flow
				.create_invoice_builder_from_invoice_request_with_keys(
					router,
					request,
					channel_manager.list_usable_channels(),
					get_payment_info,
					payment_metadata,
				)
				.map_err(InvoiceBuildError::Semantic)?;
			let builder = if allow_mpp { builder } else { builder.disallow_mpp() };
			builder
				.build_and_sign(secp_ctx)
				.map_err(|error| InvoiceBuildError::Response(InvoiceError::from(error)))
				.map(|invoice| (invoice, context))
		},
		InvoiceRequestVerifiedFromOffer::ExplicitKeys(request) => {
			let (builder, context) = flow
				.create_invoice_builder_from_invoice_request_without_keys(
					router,
					request,
					channel_manager.list_usable_channels(),
					get_payment_info,
					payment_metadata,
				)
				.map_err(InvoiceBuildError::Semantic)?;
			let builder = if allow_mpp { builder } else { builder.disallow_mpp() };
			let invoice = builder
				.build()
				.map_err(|error| InvoiceBuildError::Response(InvoiceError::from(error)))?;
			invoice
				.sign(|invoice: &UnsignedBolt12Invoice| keys_manager.sign_bolt12_invoice(invoice))
				.map_err(|error| InvoiceBuildError::Response(InvoiceError::from(error)))
				.map(|invoice| (invoice, context))
		},
	}
}

fn jit_invoice_request(
	invoice_request: &InvoiceRequestVerifiedFromOffer,
) -> Result<JitInvoiceRequest, Bolt12SemanticError> {
	jit_invoice_request_from_fields(
		invoice_request.amount(),
		invoice_request.amount_msats(),
		invoice_request.absolute_expiry().map(|expiry| expiry.as_secs()),
	)
}

fn jit_invoice_request_from_fields(
	offer_amount: Option<Amount>, amount_msat: Option<u64>, absolute_expiry: Option<u64>,
) -> Result<JitInvoiceRequest, Bolt12SemanticError> {
	let amount_msat = amount_msat.ok_or(Bolt12SemanticError::MissingAmount)?;
	match offer_amount {
		Some(Amount::Bitcoin { .. }) => {
			Ok(JitInvoiceRequest::Fixed { amount_msat, absolute_expiry })
		},
		Some(Amount::Currency { .. }) => Err(Bolt12SemanticError::UnsupportedCurrency),
		None => Ok(JitInvoiceRequest::Variable { amount_msat, absolute_expiry }),
	}
}

impl OffersMessageHandler for NodeOffersMessageHandler {
	fn handle_message(
		&self, message: OffersMessage, context: Option<OffersContext>, responder: Option<Responder>,
	) -> Option<(OffersMessage, ResponseInstruction)> {
		let invoice_request = match message {
			OffersMessage::InvoiceRequest(invoice_request) => invoice_request,
			message => return self.channel_manager.handle_message(message, context, responder),
		};

		if matches!(context, Some(OffersContext::StaticInvoiceRequested { .. })) {
			return self.channel_manager.handle_message(
				OffersMessage::InvoiceRequest(invoice_request),
				context,
				responder,
			);
		}

		let responder = responder?;
		let payment_metadata = match context.as_ref() {
			Some(OffersContext::InvoiceRequest { payment_metadata, .. }) => {
				payment_metadata.clone()
			},
			_ => None,
		};
		let invoice_request = match self.flow.verify_invoice_request(invoice_request, context) {
			Ok(InvreqResponseInstructions::SendInvoice(invoice_request)) => invoice_request,
			Ok(InvreqResponseInstructions::SendStaticInvoice { .. }) | Err(()) => return None,
		};

		let allow_mpp = invoice_request.amount().is_some();
		let payment_info = Arc::new(InvoicePaymentInfo::new());
		let result = build_invoice(
			self.flow.as_ref(),
			self.channel_manager.as_ref(),
			self.keys_manager.as_ref(),
			self.router.as_ref(),
			self.secp_ctx.as_ref(),
			&invoice_request,
			payment_metadata.clone(),
			allow_mpp,
			payment_info.as_ref(),
		);

		match result {
			Ok((invoice, context)) => {
				return Some((
					OffersMessage::Invoice(invoice),
					responder.respond_with_reply_path(context),
				));
			},
			Err(InvoiceBuildError::Semantic(Bolt12SemanticError::MissingPaths)) => {},
			Err(error) => {
				return Some((
					OffersMessage::InvoiceError(error.into_invoice_error()),
					responder.respond(),
				));
			},
		}

		// Only an actual lack of ordinary blinded paths triggers LSPS2. Negotiation is kept out of
		// the synchronous message-handler and router APIs; the verified request and reply path are
		// moved into a future which sends the response directly when its single-use lease is ready.
		let dependencies = match self.jit_dependencies.get() {
			Some(dependencies) => dependencies,
			None => {
				return Some((
					OffersMessage::InvoiceError(InvoiceError::from(
						Bolt12SemanticError::MissingPaths,
					)),
					responder.respond(),
				));
			},
		};
		let jit_request = match jit_invoice_request(&invoice_request) {
			Ok(request) => request,
			Err(error) => {
				return Some((
					OffersMessage::InvoiceError(InvoiceError::from(error)),
					responder.respond(),
				));
			},
		};
		let connection_manager = match dependencies.connection_manager.upgrade() {
			Some(connection_manager) => connection_manager,
			None => {
				return Some((
					OffersMessage::InvoiceError(InvoiceError::from_string(
						"JIT invoice handling is unavailable".to_owned(),
					)),
					responder.respond(),
				));
			},
		};
		let onion_messenger = match dependencies.onion_messenger.upgrade() {
			Some(onion_messenger) => onion_messenger,
			None => {
				return Some((
					OffersMessage::InvoiceError(InvoiceError::from_string(
						"JIT invoice handling is unavailable".to_owned(),
					)),
					responder.respond(),
				));
			},
		};

		let lsps2_client = Arc::clone(&dependencies.lsps2_client);
		let flow = Arc::clone(&self.flow);
		let channel_manager = Arc::clone(&self.channel_manager);
		let keys_manager = Arc::clone(&self.keys_manager);
		let router = Arc::clone(&self.router);
		let secp_ctx = Arc::clone(&self.secp_ctx);
		let logger = Arc::clone(&self.logger);
		dependencies.runtime.spawn_cancellable_background_task(async move {
			let response =
				lsps2_client.prepare_invoice_response(jit_request, connection_manager).await;
			let (message, instructions) = match response {
				Ok(JitInvoiceResponse { payment_metadata: jit_metadata, allow_mpp }) => {
					debug_assert_eq!(allow_mpp, jit_request.allow_mpp());
					let mut merged_metadata = payment_metadata.unwrap_or_default();
					merged_metadata.extend(jit_metadata);
					match build_invoice(
						flow.as_ref(),
						channel_manager.as_ref(),
						keys_manager.as_ref(),
						router.as_ref(),
						secp_ctx.as_ref(),
						&invoice_request,
						Some(merged_metadata),
						allow_mpp,
						payment_info.as_ref(),
					) {
						Ok((invoice, context)) => (
							OffersMessage::Invoice(invoice),
							responder.respond_with_reply_path(context),
						),
						Err(error) => (
							OffersMessage::InvoiceError(error.into_invoice_error()),
							responder.respond(),
						),
					}
				},
				Err(error) => {
					log_error!(logger, "Failed preparing LSPS2 invoice response: {}", error);
					(
						OffersMessage::InvoiceError(InvoiceError::from_string(
							"Failed preparing JIT invoice".to_owned(),
						)),
						responder.respond(),
					)
				},
			};
			if let Err(error) = onion_messenger.handle_onion_message_response(message, instructions)
			{
				log_error!(logger, "Failed sending LSPS2 invoice response: {:?}", error);
			}
		});
		None
	}

	fn release_pending_messages(&self) -> Vec<(OffersMessage, MessageSendInstructions)> {
		self.channel_manager.release_pending_messages()
	}
}

impl Confirm for NodeOffersMessageHandler {
	fn transactions_confirmed(&self, _header: &Header, _txdata: &TransactionData, _height: u32) {}

	fn transaction_unconfirmed(&self, _txid: &Txid) {}

	fn best_block_updated(&self, header: &Header, height: u32) {
		let best_block = BlockLocator::new(header.block_hash(), height);
		*self.best_block.write().expect("lock") = best_block;
		self.flow.best_block_updated(header, height);
	}

	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		Vec::new()
	}
}

impl Listen for NodeOffersMessageHandler {
	fn filtered_block_connected(&self, header: &Header, _txdata: &TransactionData, height: u32) {
		self.best_block_updated(header, height);
	}

	fn block_connected(&self, block: &Block, height: u32) {
		self.best_block_updated(&block.header, height);
	}

	fn blocks_disconnected(&self, fork_point_block: BlockLocator) {
		*self.best_block.write().expect("lock") = fork_point_block;
	}
}

#[cfg(test)]
mod tests {
	use std::num::NonZeroU64;

	use bitcoin::secp256k1::{PublicKey, SecretKey};
	use lightning::ln::channelmanager::PaymentId;
	use lightning::ln::inbound_payment::ExpandedKey;
	use lightning::offers::nonce::Nonce;
	use lightning::offers::offer::{OfferBuilder, Quantity};

	use super::*;

	struct FixedEntropy;

	impl EntropySource for FixedEntropy {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			[42; 32]
		}
	}

	fn recipient_pubkey() -> PublicKey {
		PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[43; 32]).unwrap())
	}

	#[test]
	fn fixed_offer_request_uses_quantity_resolved_amount() {
		let expanded_key = ExpandedKey::new([44; 32]);
		let nonce = Nonce::from_entropy_source(&FixedEntropy);
		let secp_ctx = Secp256k1::new();
		let offer = OfferBuilder::new(recipient_pubkey())
			.amount_msats(1_000)
			.supported_quantity(Quantity::Bounded(NonZeroU64::new(10).unwrap()))
			.build()
			.unwrap();
		let invoice_request = offer
			.request_invoice(&expanded_key, nonce, &secp_ctx, PaymentId([45; 32]))
			.unwrap()
			.quantity(3)
			.unwrap()
			.build_and_sign()
			.unwrap();

		assert_eq!(
			jit_invoice_request_from_fields(offer.amount(), invoice_request.amount_msats(), None,)
				.unwrap(),
			JitInvoiceRequest::Fixed { amount_msat: 3_000, absolute_expiry: None }
		);
	}

	#[test]
	fn variable_offer_request_disables_mpp() {
		let expanded_key = ExpandedKey::new([46; 32]);
		let nonce = Nonce::from_entropy_source(&FixedEntropy);
		let secp_ctx = Secp256k1::new();
		let offer = OfferBuilder::new(recipient_pubkey()).build().unwrap();
		let invoice_request = offer
			.request_invoice(&expanded_key, nonce, &secp_ctx, PaymentId([47; 32]))
			.unwrap()
			.amount_msats(2_500)
			.unwrap()
			.build_and_sign()
			.unwrap();
		let request =
			jit_invoice_request_from_fields(offer.amount(), invoice_request.amount_msats(), None)
				.unwrap();

		assert_eq!(
			request,
			JitInvoiceRequest::Variable { amount_msat: 2_500, absolute_expiry: None }
		);
		assert!(!request.allow_mpp());
	}
}
